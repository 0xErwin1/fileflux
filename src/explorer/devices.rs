//! The Devices section's behavior: which devices exist right now, what a
//! mount, unmount or eject does around the explorer's own state, and the
//! guards that keep them from corrupting data.
//!
//! The decisions are pure functions over plain data (which running
//! operations use a device, which tabs have to move, which queued ejects may
//! start, which menu rows a device offers); the `impl Explorer` below only
//! carries them out. Every `udisksctl` and `lsblk` call runs off the UI
//! thread through [`fileflux::core::devices`].

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use fileflux::core::Location;
use fileflux::core::devices::{
    BlockNode, Device, DeviceError, DevicePlan, DeviceState, devices, eject_plan, fallback_target,
    find_tool, gio_unmount_plan, list_blocks, mount, path_is_on_mounts, path_uses_mounts,
    real_location, run_power_off, run_unmount, unmount_plan, unmount_program,
};
use fileflux::core::places::{FuseRoots, Volume, VolumeKind, discover_volumes};
use fileflux::core::watch::{
    DeviceRoots, DeviceSource, DeviceWatcher, RefreshGate, safety_poll_due,
};
use gpui::{ClipboardItem, Context, Window};

use super::ops::{
    DialogAction, DialogKind, Job, MenuCommand, MenuItemSpec, MenuRow, Severity, UndoAction,
};
use super::state::Transition;
use super::view::{Explorer, TabId};

/// How long an unmount waits for the tabs on the device to move away: the
/// number of steps and the time of each.
const TAB_WAIT_STEPS: usize = 60;
const TAB_WAIT_STEP: Duration = Duration::from_millis(50);

/// What a device's menu and row offer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DeviceActions {
    pub(crate) mount: bool,
    pub(crate) unmount: bool,
    pub(crate) eject: bool,
}

/// The actions of `device`, none of them when `udisksctl` is missing.
pub(crate) fn actions_for(device: &Device, udisks: bool) -> DeviceActions {
    if !udisks {
        return DeviceActions::default();
    }
    DeviceActions {
        mount: device.can_mount(),
        unmount: device.can_unmount(),
        eject: device.can_eject(),
    }
}

fn command_row(command: MenuCommand, label: &'static str) -> MenuRow {
    MenuRow::Item(MenuItemSpec::new(command, label))
}

fn device_command_rows(actions: DeviceActions) -> Vec<MenuRow> {
    let mut rows = Vec::new();
    if actions.mount {
        rows.push(command_row(MenuCommand::Mount, "Mount"));
    }
    if actions.unmount {
        rows.push(command_row(MenuCommand::Unmount, "Unmount"));
    }
    if actions.eject {
        rows.push(command_row(MenuCommand::Eject, "Eject"));
    }
    rows
}

/// A mounted row's menu: the place rows with Unmount and Eject in a group of
/// their own after the open rows.
pub(crate) fn with_device_rows(mut rows: Vec<MenuRow>, actions: DeviceActions) -> Vec<MenuRow> {
    let mut group = device_command_rows(actions);
    if group.is_empty() {
        return rows;
    }
    group.push(MenuRow::Separator);
    let at = rows
        .iter()
        .position(|row| matches!(row, MenuRow::Separator))
        .map_or(rows.len(), |separator| separator + 1);
    rows.splice(at..at, group);
    rows
}

/// An unmounted or locked row's menu: Mount, Eject, Copy path.
pub(crate) fn device_menu_rows(actions: DeviceActions) -> Vec<MenuRow> {
    let mut rows = device_command_rows(actions);
    if !rows.is_empty() {
        rows.push(MenuRow::Separator);
    }
    rows.push(command_row(MenuCommand::CopyPath, "Copy path"));
    rows
}

/// The muted line under an unmounted row.
pub(crate) fn caption(device: &Device, working: Option<&'static str>) -> &'static str {
    match (working, &device.state) {
        (Some(word), _) => word,
        (None, DeviceState::Locked) => "Locked",
        (None, _) => "Not mounted",
    }
}

fn undo_paths(action: &UndoAction) -> Vec<PathBuf> {
    match action {
        UndoAction::Rename { old, new, .. } => vec![old.clone(), new.clone()],
        UndoAction::Trash { items } => items
            .iter()
            .flat_map(|entry| [entry.item.original.clone(), entry.item.files_path()])
            .collect(),
        UndoAction::Create { path, .. } => vec![path.clone()],
        UndoAction::Copy { items } | UndoAction::Move { items } => items
            .iter()
            .flat_map(|item| [item.from.clone(), item.to.clone()])
            .collect(),
    }
}

/// Everything one running operation reads or writes. The Trash jobs name
/// the home Trash: what they restore or delete is read from it.
pub(crate) fn job_paths(job: &Job, trash_root: &Path) -> Vec<PathBuf> {
    match job {
        Job::Create { dir, .. } => vec![dir.clone()],
        Job::Rename { path, .. } => vec![path.clone()],
        Job::Trash(paths) | Job::DeletePermanently(paths) => paths.clone(),
        Job::Restore(_) | Job::DeleteFromTrash(_) | Job::EmptyTrash(_) => {
            vec![trash_root.to_path_buf()]
        }
        Job::Undo(action) => undo_paths(action),
        Job::Paste(paste) => {
            let mut paths = paste.sources.clone();
            paths.push(paste.dest.clone());
            paths
        }
    }
}

/// The mounted devices the mount table did not list (a mount point outside
/// the roots it shows, for one), as volumes of their own so a device that
/// was just mounted never vanishes from the sidebar. No usage numbers: they
/// need a `statvfs` off the UI thread.
pub(crate) fn unlisted_volumes(volumes: &[Volume], devices: &[Device]) -> Vec<Volume> {
    devices
        .iter()
        .filter_map(|device| {
            let mount = device.mount_point()?;
            let listed = volumes.iter().any(|volume| volume.path == mount);
            (!listed).then(|| Volume {
                label: device.label.clone(),
                path: mount.to_path_buf(),
                removable: device.ejectable,
                usage: None,
                kind: VolumeKind::Disk,
            })
        })
        .collect()
}

/// The ids of the running operations (id, paths they touch) that use one
/// of `mounts`, by real location: a destination reached through a symlink,
/// or one that does not exist yet, counts. Reads the filesystem: call it off
/// the UI thread.
pub(crate) fn busy_ops(running: &[(u64, Vec<PathBuf>)], mounts: &[PathBuf]) -> Vec<u64> {
    running
        .iter()
        .filter(|(_, paths)| paths.iter().any(|path| path_is_on_mounts(path, mounts)))
        .map(|(id, _)| *id)
        .collect()
}

/// Whether the plan may still unmount: `Err` with the reason when an
/// operation that touches its mounts is running now. The plan checks this
/// right before every unmount, because the operations it waited for are not
/// the only ones that can exist by then.
pub(crate) fn recheck(running: &[(u64, Vec<PathBuf>)], plan: &DevicePlan) -> Result<(), String> {
    match busy_ops(running, &plan.mounts()).len() {
        0 => Ok(()),
        1 => Err("an operation started using it".to_owned()),
        count => Err(format!("{count} operations started using it")),
    }
}

/// A navigation step whose destination passed the device check. The step
/// that runs once the check has finished is built only here, from the
/// location that was checked, never re-derived from the history as it is by
/// then (two quick Back presses would otherwise both take the second step,
/// which nobody checked).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CheckedStep(Transition);

impl CheckedStep {
    /// The step to take to `checked` once its check ended. `now` is what
    /// `transition` would reach now: when that still is `checked` the
    /// transition runs as asked (history keeps its meaning); when the
    /// history moved meanwhile, the step visits `checked` instead.
    pub(crate) fn of(transition: Transition, now: Option<Location>, checked: Location) -> Self {
        if now.as_ref() == Some(&checked) {
            Self(transition)
        } else {
            Self(Transition::Visit(checked))
        }
    }

    pub(crate) fn into_transition(self) -> Transition {
        self.0
    }
}

/// The toast that refuses new work on a device being unmounted or ejected,
/// when any of `paths` (real locations, or plain text for a first quick
/// look) lies on one of the `draining` plans' mounts.
pub(crate) fn refusal<'a>(
    draining: impl IntoIterator<Item = &'a DevicePlan>,
    paths: &[PathBuf],
) -> Option<String> {
    draining.into_iter().find_map(|plan| {
        let mounts = plan.mounts();
        paths
            .iter()
            .any(|path| path_uses_mounts(path, &mounts))
            .then(|| {
                let verb = if plan.power_off.is_some() {
                    "ejected"
                } else {
                    "unmounted"
                };
                format!("{} is being {verb}", plan.label)
            })
    })
}

/// The tabs (id, shown folder, folder being opened) that have to leave
/// `mounts`, with the folder each one leaves: the one being opened when
/// that is on a mount, else the shown one.
pub(crate) fn tabs_to_move(
    tabs: &[(TabId, Option<PathBuf>, Option<PathBuf>)],
    mounts: &[PathBuf],
) -> Vec<(TabId, PathBuf)> {
    tabs.iter()
        .filter_map(|(id, shown, opening)| {
            [opening, shown]
                .into_iter()
                .flatten()
                .find(|path| path_is_on_mounts(path, mounts))
                .map(|path| (*id, path.clone()))
        })
        .collect()
}

/// Where every tab that has to leave `mounts` goes: (tab, folder it leaves,
/// folder it moves to). Reads the filesystem: call it off the UI thread.
pub(crate) fn plan_tab_moves(
    tabs: &[(TabId, Option<PathBuf>, Option<PathBuf>)],
    mounts: &[PathBuf],
    home: Option<&Path>,
) -> Vec<(TabId, PathBuf, PathBuf)> {
    tabs_to_move(tabs, mounts)
        .into_iter()
        .map(|(id, leaving)| {
            let target = fallback_target(&leaving, mounts, home, Path::exists);
            (id, leaving, target)
        })
        .collect()
}

/// What a tab shows while its device is being unmounted. The outcome is not
/// known yet: [`moved_notice`] or [`failed_notice`] replace it.
pub(crate) fn leaving_notice(label: &str) -> String {
    format!("Unmounting {label}\u{2026}")
}

/// The notice a tab shows after it left an unmounted device.
pub(crate) fn moved_notice(label: &str, target: &Path) -> String {
    format!("{label} was unmounted; showing {}", target.display())
}

/// The notice a tab that left the device keeps when the unmount failed. The
/// tab stays where it went: moving it back would put an inotify watch on
/// the device again, and the user already chose to leave it.
pub(crate) fn failed_notice(label: &str, reason: &str) -> String {
    format!("Could not unmount {label}; {reason}")
}

/// The toast a finished plan ends with. "Can be removed safely" only when
/// every step ran, the power-off included.
pub(crate) fn plan_notice(
    plan: &DevicePlan,
    result: &Result<(), DeviceError>,
) -> (Severity, String) {
    let eject = plan.power_off.is_some();
    match result {
        Ok(()) if eject => (
            Severity::Success,
            format!("{} can be removed safely", plan.label),
        ),
        Ok(()) => (Severity::Success, format!("Unmounted {}", plan.label)),
        Err(error) => {
            let verb = if eject { "eject" } else { "unmount" };
            (
                Severity::Error,
                format!("Could not {verb} {}: {error}", plan.label),
            )
        }
    }
}

/// The toast of "wait and eject" while operations still use the device;
/// `None` when none does and the plan just starts.
pub(crate) fn queued_notice(plan: &DevicePlan, count: usize) -> Option<String> {
    if count == 0 {
        return None;
    }
    let verb = if plan.power_off.is_some() {
        "eject"
    } else {
        "unmount"
    };
    Some(format!(
        "Will {verb} {} when {count} operation{} finish{}",
        plan.label,
        if count == 1 { "" } else { "s" },
        if count == 1 { "es" } else { "" },
    ))
}

/// Whether the device list is re-read now: never while a read is running,
/// at once the first time (or after `last` was cleared to force one), then
/// when the watcher's events have settled (`gate`), and only while a watch
/// is missing (`degraded`) on the slow fallback poll. There is no polling
/// while the watches work, focused or not.
pub(crate) fn refresh_due(
    gate: &RefreshGate,
    last: Option<Instant>,
    now: Instant,
    degraded: bool,
    in_flight: bool,
) -> bool {
    if in_flight {
        return false;
    }
    last.is_none() || gate.ready(now) || safety_poll_due(last, now, degraded)
}

/// What a gvfs mount's menu and row offer: only Unmount, and only when
/// `gio` is installed to do it. Never Mount or Eject: a phone is neither a
/// block device nor powered off from here.
pub(crate) fn gvfs_actions(kind: VolumeKind, gio: bool) -> DeviceActions {
    DeviceActions {
        mount: false,
        unmount: gio && kind.is_gvfs(),
        eject: false,
    }
}

/// Unmounting the gvfs mount at `path` through `gio`: `None` without `gio`,
/// or when `path` is not a gvfs volume of `volumes`.
pub(crate) fn gvfs_unmount_plan(
    volumes: &[Volume],
    path: &Path,
    gio: Option<&Path>,
) -> Option<DevicePlan> {
    gio?;
    let volume = volumes
        .iter()
        .find(|volume| volume.path == path && volume.kind.is_gvfs())?;
    Some(gio_unmount_plan(&volume.label, &volume.path))
}

/// Queues `plan` to run when its device is idle; `false` when the same plan
/// already waits.
pub(crate) fn queue_plan(pending: &mut Vec<DevicePlan>, plan: DevicePlan) -> bool {
    if pending.contains(&plan) {
        return false;
    }
    pending.push(plan);
    true
}

/// Removes and returns the queued plans whose mounts nothing uses any more,
/// in the order they were queued.
pub(crate) fn take_ready(
    pending: &mut Vec<DevicePlan>,
    is_busy: impl Fn(&[PathBuf]) -> bool,
) -> Vec<DevicePlan> {
    let (waiting, ready) = std::mem::take(pending)
        .into_iter()
        .partition(|plan| is_busy(&plan.mounts()));
    *pending = waiting;
    ready
}

/// What the sidebar and the actions know about the machine's devices.
#[derive(Debug, Default)]
pub(crate) struct DeviceHub {
    /// `udisksctl`, when installed: without it nothing can be mounted.
    pub(crate) udisksctl: Option<PathBuf>,
    /// `lsblk`, when installed: without it only mounted volumes show.
    pub(crate) lsblk: Option<PathBuf>,
    /// The tree the last read returned.
    pub(crate) nodes: Vec<BlockNode>,
    /// The mountable devices of that tree.
    pub(crate) list: Vec<Device>,
    /// Devices an action is working on now, with the word for the row.
    pub(crate) working: HashMap<PathBuf, &'static str>,
    /// Unmounts and ejects waiting for running operations to end.
    pub(crate) pending: Vec<DevicePlan>,
    /// Unmounts and ejects running now.
    pub(crate) active: Vec<DevicePlan>,
    /// Whether the queued plans are being checked against the operations.
    pub(crate) checking: bool,
    /// When the list was last read; `None` forces a read at the next tick.
    pub(crate) last_poll: Option<Instant>,
    /// A read is running.
    pub(crate) polling: bool,
    /// `gio`, when installed: the only way to unmount a gvfs mount.
    pub(crate) gio: Option<PathBuf>,
    /// The thread that reports device changes; `None` when inotify is
    /// unavailable.
    pub(crate) watcher: Option<DeviceWatcher>,
    /// What the watcher reported, drained at every tick.
    pub(crate) signals: Option<Receiver<DeviceSource>>,
    /// The events seen and not yet read.
    pub(crate) gate: RefreshGate,
    /// Whether the window had the focus at the last tick.
    pub(crate) was_active: bool,
    /// The watcher's channel closed: its thread stopped.
    pub(crate) signals_closed: bool,
}

impl DeviceHub {
    /// Looks for `udisksctl`, `lsblk` and `gio` on `$PATH` and starts the
    /// watcher on `/dev`, the mount table and the gvfs directory.
    pub(crate) fn from_env() -> Self {
        let path = std::env::var_os("PATH");
        let (watcher, signals) = match DeviceWatcher::new(&DeviceRoots::system()) {
            Ok((watcher, signals)) => (Some(watcher), Some(signals)),
            Err(error) => {
                log::warn!("device events are unavailable, polling slowly instead: {error}");
                (None, None)
            }
        };
        Self {
            udisksctl: find_tool("udisksctl", path.as_deref()),
            lsblk: find_tool("lsblk", path.as_deref()),
            gio: find_tool("gio", path.as_deref()),
            watcher,
            signals,
            was_active: true,
            ..Self::default()
        }
    }

    /// Whether the slow fallback poll is needed: no watcher, or a watch
    /// that could not be established or was lost.
    pub(crate) fn degraded(&self) -> bool {
        self.signals_closed || self.watcher.as_ref().is_none_or(DeviceWatcher::degraded)
    }

    /// Moves what the watcher reported into the gate.
    pub(crate) fn collect_signals(&mut self, now: Instant) {
        let Some(signals) = &self.signals else {
            return;
        };
        loop {
            match signals.try_recv() {
                Ok(source) => {
                    log::debug!("device event: {source:?}");
                    self.gate.note(now, source);
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.signals_closed = true;
                    return;
                }
            }
        }
    }

    /// A window that just got the focus back looks at the devices again: a
    /// change the kernel could not announce (a FUSE directory only tells
    /// about what happens through it) shows up at the latest then.
    pub(crate) fn note_focus(&mut self, active: bool, now: Instant) {
        if active && !self.was_active {
            self.gate.note(now, DeviceSource::Mounts);
        }
        self.was_active = active;
    }

    /// The one-line notice under the Devices label when a tool is missing.
    pub(crate) fn notice(&self) -> Option<&'static str> {
        if self.udisksctl.is_none() {
            Some("udisksctl was not found: mounting and ejecting are unavailable")
        } else if self.lsblk.is_none() {
            Some("lsblk was not found: devices that are not mounted are not listed")
        } else {
            None
        }
    }

    /// Whether a device is going away: a plan is queued or running.
    pub(crate) fn is_draining(&self) -> bool {
        !self.pending.is_empty() || !self.active.is_empty()
    }

    /// The plans of devices that are going away, queued or running: no new
    /// work may start on their mounts.
    pub(crate) fn draining(&self) -> impl Iterator<Item = &DevicePlan> {
        self.pending.iter().chain(&self.active)
    }

    /// The device with block node `path`.
    pub(crate) fn find(&self, path: &Path) -> Option<&Device> {
        self.list.iter().find(|device| device.path == path)
    }

    /// The word an action in progress shows on the row of `device`.
    pub(crate) fn working_on(&self, device: &Device) -> Option<&'static str> {
        self.working
            .get(&device.path)
            .or_else(|| self.working.get(&device.disk))
            .copied()
    }
}

/// The folder of a location, when it is one.
fn directory_of(location: &Location) -> Option<PathBuf> {
    match location {
        Location::Directory(path) => Some(path.clone()),
        Location::Trash | Location::Recent => None,
    }
}

impl Explorer {
    /// What the device with block node `path` offers right now.
    pub(crate) fn device_actions(&self, path: &Path) -> Option<DeviceActions> {
        if let Some(device) = self.devices.find(path) {
            return Some(actions_for(device, self.devices.udisksctl.is_some()));
        }
        let volume = self.gvfs_volume(path)?;
        Some(gvfs_actions(volume.kind, self.devices.gio.is_some()))
    }

    /// The gvfs mount at `path` among the sidebar's volumes.
    fn gvfs_volume(&self, path: &Path) -> Option<&Volume> {
        self.places
            .as_ref()?
            .volumes
            .iter()
            .find(|volume| volume.path == path && volume.kind.is_gvfs())
    }

    /// Runs on every pump tick: re-reads the devices when the watcher's
    /// events have settled and starts the queued unmounts whose operations
    /// have ended.
    pub(crate) fn pump_devices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        self.devices.collect_signals(now);
        self.devices.note_focus(window.is_window_active(), now);
        if self.places.is_some()
            && refresh_due(
                &self.devices.gate,
                self.devices.last_poll,
                now,
                self.devices.degraded(),
                self.devices.polling,
            )
        {
            self.refresh_devices(now, cx);
        }
        self.start_ready_plans(window, cx);
    }

    /// Reads the mount table and `lsblk` on the background executor and
    /// applies what changed.
    fn refresh_devices(&mut self, now: Instant, cx: &mut Context<Self>) {
        self.devices.polling = true;
        self.devices.last_poll = Some(now);
        self.devices.gate.start(now);
        let home = self.home.clone();
        let lsblk = self.devices.lsblk.clone();
        let read = cx.background_executor().spawn(async move {
            let volumes = discover_volumes(home.as_deref());
            // Read here, not at the next delete: resolving the gvfs
            // directory can block, and that must never be the UI thread.
            let roots = FuseRoots::system();
            let blocks = lsblk.map(|program| list_blocks(&program));
            (volumes, roots, blocks)
        });
        cx.spawn(async move |this, cx| {
            let (volumes, roots, blocks) = read.await;
            if let Err(error) = this.update(cx, |this, cx| {
                this.apply_device_read(volumes, roots, blocks, cx)
            }) {
                log::debug!("the explorer closed before the devices were read: {error}");
            }
        })
        .detach();
    }

    fn apply_device_read(
        &mut self,
        volumes: io::Result<Vec<Volume>>,
        roots: FuseRoots,
        blocks: Option<Result<Vec<BlockNode>, DeviceError>>,
        cx: &mut Context<Self>,
    ) {
        self.devices.polling = false;
        let mut changed = false;
        if let Some(places) = self.places.as_mut() {
            places.fuse = roots;
        }
        match volumes {
            Ok(volumes) => {
                if let Some(places) = self.places.as_mut()
                    && places.volumes != volumes
                {
                    log::info!(
                        "devices updated: {}",
                        volumes
                            .iter()
                            .map(|volume| volume.label.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    places.volumes = volumes;
                    changed = true;
                }
            }
            Err(error) => log::debug!("the mount table could not be re-read: {error}"),
        }
        match blocks {
            Some(Ok(nodes)) => {
                if nodes != self.devices.nodes {
                    self.devices.list = devices(&nodes);
                    self.devices.nodes = nodes;
                    changed = true;
                }
            }
            // The last good list stays; a failing lsblk is not worth a toast
            // on every device event.
            Some(Err(error)) => log::debug!("the device list could not be read: {error}"),
            None => {}
        }
        if changed {
            cx.notify();
        }
    }

    /// A click on the row of a device that is not mounted: mounts it, or
    /// says why it cannot be.
    pub(crate) fn activate_device(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.devices.find(&path).map(|device| device.state.clone()) {
            Some(DeviceState::Unmounted) => self.mount_device(path, window, cx),
            Some(DeviceState::Locked) => {
                let label = self
                    .devices
                    .find(&path)
                    .map(|device| device.label.clone())
                    .unwrap_or_default();
                self.say(
                    Severity::Info,
                    format!("{label} is encrypted. Unlocking is not supported yet"),
                    window,
                    cx,
                );
            }
            Some(DeviceState::Mounted(mount)) => {
                self.navigate(Transition::Visit(Location::Directory(mount)), window, cx);
            }
            None => {}
        }
    }

    /// What a device menu item does to the device with block node `path`.
    pub(crate) fn run_device_command(
        &mut self,
        command: MenuCommand,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            MenuCommand::Mount => self.mount_device(path, window, cx),
            MenuCommand::Unmount => self.unmount_device(&path, window, cx),
            MenuCommand::Eject => self.eject_device(&path, window, cx),
            MenuCommand::CopyPath => {
                cx.write_to_clipboard(ClipboardItem::new_string(
                    path.to_string_lossy().into_owned(),
                ));
                self.tab_mut()
                    .state
                    .set_notice("Copied the path".to_owned());
                cx.notify();
            }
            _ => {}
        }
    }

    /// `udisksctl mount`, off the UI thread; on success the tab goes to the
    /// mount point.
    pub(crate) fn mount_device(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(udisksctl) = self.devices.udisksctl.clone() else {
            return;
        };
        let Some(device) = self.devices.find(&path).filter(|device| device.can_mount()) else {
            return;
        };
        let label = device.label.clone();
        if self.devices.working.contains_key(&path) {
            return;
        }
        self.devices
            .working
            .insert(path.clone(), "Mounting\u{2026}");
        cx.notify();
        let device = path.clone();
        let run = cx
            .background_executor()
            .spawn(async move { mount(&udisksctl, &device) });
        cx.spawn_in(window, async move |this, cx| {
            let result = run.await;
            if let Err(error) = this.update_in(cx, |this, window, cx| {
                this.devices.working.remove(&path);
                this.devices.last_poll = None;
                match result {
                    Ok(mount_point) => {
                        this.navigate(
                            Transition::Visit(Location::Directory(mount_point)),
                            window,
                            cx,
                        );
                    }
                    Err(error) => {
                        this.say(
                            Severity::Error,
                            format!("Could not mount {label}: {error}"),
                            window,
                            cx,
                        );
                    }
                }
                cx.notify();
            }) {
                log::debug!("the explorer closed before a mount finished: {error}");
            }
        })
        .detach();
    }

    /// Unmounts the device with block node `path` (after the guards).
    pub(crate) fn unmount_device(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let plan = self.devices.find(path).and_then(unmount_plan).or_else(|| {
            let volumes = &self.places.as_ref()?.volumes;
            gvfs_unmount_plan(volumes, path, self.devices.gio.as_deref())
        });
        match plan {
            Some(plan) => self.request_device_plan(plan, window, cx),
            None if self.gvfs_volume(path).is_some() => self.say(
                Severity::Info,
                "Unmount it from your system's file manager",
                window,
                cx,
            ),
            None => self.say(
                Severity::Info,
                "That device cannot be unmounted here",
                window,
                cx,
            ),
        }
    }

    /// Ejects the disk of the device with block node `path` (after the
    /// guards): every partition unmounted, then powered off.
    pub(crate) fn eject_device(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let plan = self
            .devices
            .find(path)
            .and_then(|device| eject_plan(&self.devices.nodes, &device.disk, &device.label));
        match plan {
            Some(plan) => self.request_device_plan(plan, window, cx),
            None => self.say(Severity::Info, "That device cannot be ejected", window, cx),
        }
    }

    /// The running operations (id, paths they touch).
    pub(crate) fn running_touches(&self) -> Vec<(u64, Vec<PathBuf>)> {
        self.op_contexts
            .iter()
            .map(|(id, context)| (*id, context.touches.clone()))
            .collect()
    }

    /// Runs `then`, unless `paths` are on a device that is being unmounted
    /// or ejected: then it says so and does nothing. Nothing is checked
    /// while no device is going away, so the usual case stays immediate.
    ///
    /// The paths are compared as written first, then by real location (a
    /// symlink into the device counts), which reads the filesystem and so
    /// runs off the UI thread. The answer is applied only after checking
    /// again against the devices going away at that moment.
    pub(crate) fn when_not_ejecting(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.check_not_ejecting(
            paths,
            window,
            cx,
            move |this, refused, window, cx| match refused {
                Some(text) => this.say(Severity::Info, text, window, cx),
                None => then(this, window, cx),
            },
        );
    }

    /// [`Self::when_not_ejecting`] that hands the verdict to `done` (the
    /// refusal text, or `None` when the paths are free) instead of saying
    /// it, for work that has to settle its own state either way. `done` runs
    /// at once while no device is going away, else right after the final
    /// check, on the UI thread: nothing can change between the two.
    pub(crate) fn check_not_ejecting(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, Option<String>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if !self.devices.is_draining() {
            return done(self, None, window, cx);
        }
        if let Some(text) = refusal(self.devices.draining(), &paths) {
            return done(self, Some(text), window, cx);
        }
        let real = cx.background_executor().spawn(async move {
            paths
                .iter()
                .map(|path| real_location(path))
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let real = real.await;
            if let Err(error) = this.update_in(cx, |this, window, cx| {
                let verdict = refusal(this.devices.draining(), &real);
                done(this, verdict, window, cx);
            }) {
                log::debug!("the explorer closed before a device check ended: {error}");
            }
        })
        .detach();
    }

    /// Unmounting while an operation writes there can corrupt data: asks
    /// first when any uses the device, else goes ahead. The check reads the
    /// filesystem, so it runs off the UI thread.
    fn request_device_plan(
        &mut self,
        plan: DevicePlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let running = self.running_touches();
        let mounts = plan.mounts();
        let busy = cx
            .background_executor()
            .spawn(async move { busy_ops(&running, &mounts) });
        cx.spawn_in(window, async move |this, cx| {
            let busy = busy.await;
            if let Err(error) = this.update_in(cx, |this, window, cx| {
                if busy.is_empty() {
                    return this.run_device_plan(plan, window, cx);
                }
                let titles: Vec<String> = busy
                    .iter()
                    .filter_map(|id| this.progress.title(*id).map(str::to_owned))
                    .collect();
                this.open_dialog(
                    DialogKind::DeviceBusy {
                        label: plan.label.clone(),
                        count: busy.len(),
                        eject: plan.power_off.is_some(),
                    },
                    &titles,
                    DialogAction::DeviceWhenIdle(plan),
                    window,
                    cx,
                );
            }) {
                log::debug!("the explorer closed before a device check ended: {error}");
            }
        })
        .detach();
    }

    /// "Wait and eject": from now on the device is draining, and the plan
    /// starts once nothing uses it (at once when nothing does).
    pub(crate) fn queue_device_plan(
        &mut self,
        plan: DevicePlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !queue_plan(&mut self.devices.pending, plan.clone()) {
            return;
        }
        let running = self.running_touches();
        let mounts = plan.mounts();
        let count = cx
            .background_executor()
            .spawn(async move { busy_ops(&running, &mounts).len() });
        cx.spawn_in(window, async move |this, cx| {
            let count = count.await;
            if let Err(error) = this.update_in(cx, |this, window, cx| {
                match queued_notice(&plan, count) {
                    Some(text) => this.say(Severity::Info, text, window, cx),
                    None => {
                        // Idle already: start unless the tick got there first.
                        let before = this.devices.pending.len();
                        this.devices.pending.retain(|queued| queued != &plan);
                        if this.devices.pending.len() < before {
                            this.run_device_plan(plan, window, cx);
                        }
                    }
                }
            }) {
                log::debug!("the explorer closed before a device check ended: {error}");
            }
        })
        .detach();
    }

    /// Starts the queued plans whose operations have ended. The check reads
    /// the filesystem, so it runs off the UI thread, one at a time.
    fn start_ready_plans(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.devices.pending.is_empty() || self.devices.checking {
            return;
        }
        self.devices.checking = true;
        let running = self.running_touches();
        let waiting: Vec<Vec<PathBuf>> = self
            .devices
            .pending
            .iter()
            .map(DevicePlan::mounts)
            .collect();
        let idle = cx.background_executor().spawn(async move {
            waiting
                .into_iter()
                .map(|mounts| {
                    let free = busy_ops(&running, &mounts).is_empty();
                    (mounts, free)
                })
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let idle = idle.await;
            if let Err(error) = this.update_in(cx, |this, window, cx| {
                this.devices.checking = false;
                // A plan queued while the check ran was not looked at: it
                // waits for the next one.
                let ready = take_ready(&mut this.devices.pending, |mounts| {
                    !idle
                        .iter()
                        .any(|(checked, free)| checked == mounts && *free)
                });
                for plan in ready {
                    this.run_device_plan(plan, window, cx);
                }
            }) {
                log::debug!("the explorer closed before a device check ended: {error}");
            }
        })
        .detach();
    }

    /// The tabs as (id, shown folder, folder being opened).
    fn tab_snapshot(&self) -> Vec<(TabId, Option<PathBuf>, Option<PathBuf>)> {
        self.tabs
            .iter()
            .map(|tab| {
                (
                    tab.id,
                    directory_of(tab.state.current()),
                    tab.state.pending_directory().map(Path::to_path_buf),
                )
            })
            .collect()
    }

    /// Runs the plan. From the moment it starts the device is draining. The
    /// tabs leave and their watches are released first; then, before every
    /// unmount, the running operations are checked once more and the plan
    /// stops if one uses the device. `udisksctl` and every read of the
    /// filesystem run off the UI thread.
    fn run_device_plan(&mut self, plan: DevicePlan, window: &mut Window, cx: &mut Context<Self>) {
        let udisksctl = self.devices.udisksctl.clone();
        let gio = self.devices.gio.clone();
        let eject = plan.power_off.is_some();
        let key = plan
            .power_off
            .clone()
            .or_else(|| plan.unmount.first().map(|step| step.device.clone()));
        if let Some(key) = &key {
            if self.devices.working.contains_key(key) {
                let text = format!("{} is already being worked on", plan.label);
                return self.say(Severity::Info, text, window, cx);
            }
            let word = if eject {
                "Ejecting\u{2026}"
            } else {
                "Unmounting\u{2026}"
            };
            self.devices.working.insert(key.clone(), word);
        }
        self.devices.active.push(plan.clone());
        let home = self.home.clone();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let mounts = plan.mounts();
            let mut moved: Vec<(TabId, PathBuf)> = Vec::new();
            let mut unmounted = false;
            let outcome: Option<Result<(), DeviceError>> = async {
                // The tabs on the device leave it.
                let tabs = this.update_in(cx, |this, _, _| this.tab_snapshot()).ok()?;
                let leaving = cx
                    .background_executor()
                    .spawn({
                        let mounts = mounts.clone();
                        async move { plan_tab_moves(&tabs, &mounts, home.as_deref()) }
                    })
                    .await;
                this.update_in(cx, |this, window, cx| {
                    for (id, _, target) in &leaving {
                        let scan = this.tabs.by_id_mut(*id).and_then(|tab| {
                            tab.state
                                .begin_fallback(target.clone(), leaving_notice(&plan.label))
                        });
                        this.start_scan_in(*id, scan, window, cx);
                    }
                })
                .ok()?;
                moved = leaving
                    .into_iter()
                    .map(|(id, _, target)| (id, target))
                    .collect();
                // The fallback scans end within a moment; wait for the tabs
                // to be gone.
                for _ in 0..TAB_WAIT_STEPS {
                    let tabs = this.update_in(cx, |this, _, _| this.tab_snapshot()).ok()?;
                    let still = cx
                        .background_executor()
                        .spawn({
                            let mounts = mounts.clone();
                            async move { !tabs_to_move(&tabs, &mounts).is_empty() }
                        })
                        .await;
                    if !still {
                        break;
                    }
                    cx.background_executor().timer(TAB_WAIT_STEP).await;
                }
                // An inotify watch on the device would keep it busy.
                let watched = this.update_in(cx, |this, _, _| this.watched_paths()).ok()?;
                let doomed = cx
                    .background_executor()
                    .spawn({
                        let mounts = mounts.clone();
                        async move {
                            watched
                                .into_iter()
                                .filter(|path| path_is_on_mounts(path, &mounts))
                                .collect::<Vec<_>>()
                        }
                    })
                    .await;
                this.update_in(cx, |this, _, _| this.release_watches(&doomed))
                    .ok()?;
                for step in &plan.unmount {
                    // Anything that started since the plan was asked for.
                    let running = this
                        .update_in(cx, |this, _, _| this.running_touches())
                        .ok()?;
                    let checked = cx
                        .background_executor()
                        .spawn({
                            let plan = plan.clone();
                            async move { recheck(&running, &plan) }
                        })
                        .await;
                    if let Err(reason) = checked {
                        return Some(Err(DeviceError::Failed { reason }));
                    }
                    let step = step.clone();
                    let program = match unmount_program(&step, udisksctl.as_deref(), gio.as_deref())
                    {
                        Ok(program) => program.to_path_buf(),
                        Err(error) => return Some(Err(error)),
                    };
                    let done = cx
                        .background_executor()
                        .spawn(async move { run_unmount(&program, &step) })
                        .await;
                    if let Err(error) = done {
                        return Some(Err(error));
                    }
                }
                unmounted = true;
                if let Some(disk) = plan.power_off.clone() {
                    let Some(program) = udisksctl.clone() else {
                        return Some(Err(DeviceError::Failed {
                            reason: "udisksctl was not found".to_owned(),
                        }));
                    };
                    let done = cx
                        .background_executor()
                        .spawn(async move { run_power_off(&program, &disk) })
                        .await;
                    return Some(done);
                }
                Some(Ok(()))
            }
            .await;
            let Some(result) = outcome else {
                return;
            };
            if let Err(error) = this.update_in(cx, |this, window, cx| {
                this.finish_device_plan(
                    &plan,
                    key.as_deref(),
                    result,
                    (&moved, unmounted),
                    window,
                    cx,
                );
            }) {
                log::debug!("the explorer closed before a device action finished: {error}");
            }
        })
        .detach();
    }

    fn finish_device_plan(
        &mut self,
        plan: &DevicePlan,
        key: Option<&Path>,
        result: Result<(), DeviceError>,
        (moved, unmounted): (&[(TabId, PathBuf)], bool),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = key {
            self.devices.working.remove(key);
        }
        if let Some(at) = self.devices.active.iter().position(|active| active == plan) {
            self.devices.active.remove(at);
        }
        // Show the new state at once instead of at the next poll.
        self.devices.last_poll = None;
        // The tabs that left say how it ended, unless the user went elsewhere.
        for (id, target) in moved {
            let text = match (&result, unmounted) {
                (_, true) => moved_notice(&plan.label, target),
                (Err(error), false) => failed_notice(&plan.label, &error.to_string()),
                (Ok(()), false) => continue,
            };
            if let Some(tab) = self.tabs.by_id_mut(*id)
                && tab.state.current() == &Location::Directory(target.clone())
            {
                tab.state.set_notice(text);
            }
        }
        let (severity, text) = plan_notice(plan, &result);
        self.say(severity, text, window, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::ops::paste::{ClipMode, ConflictLink, PasteJob};
    use crate::explorer::ops::{Identity, NewKind, UndoAction};
    use fileflux::core::devices::{DeviceState, UnmountStep, UnmountVia};
    use std::ffi::OsString;
    use std::sync::mpsc::channel;

    fn p(text: &str) -> PathBuf {
        PathBuf::from(text)
    }

    fn device(state: DeviceState, ejectable: bool) -> Device {
        let mounts = match &state {
            DeviceState::Mounted(mount) => vec![mount.clone()],
            _ => Vec::new(),
        };
        Device {
            path: p("/dev/sdb1"),
            disk: p("/dev/sdb"),
            label: "KINGSTON".to_owned(),
            fstype: Some("vfat".to_owned()),
            size: 15_000_000_000,
            state,
            mounts,
            ejectable,
        }
    }

    fn mounted(ejectable: bool) -> Device {
        device(DeviceState::Mounted(p("/run/media/u/KINGSTON")), ejectable)
    }

    fn labels(rows: &[MenuRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                MenuRow::Separator => "-".to_owned(),
                MenuRow::Header(label) => format!("# {label}"),
                MenuRow::Item(item) => item.label.to_owned(),
            })
            .collect()
    }

    fn plan(label: &str, mount: &str) -> DevicePlan {
        DevicePlan {
            label: label.to_owned(),
            unmount: vec![UnmountStep {
                device: p("/dev/sdb1"),
                mounts: vec![p(mount)],
                via: UnmountVia::Udisks,
            }],
            power_off: Some(p("/dev/sdb")),
        }
    }

    // --- row and menu model ---------------------------------------------------

    #[test]
    fn actions_follow_the_device_and_vanish_without_udisksctl() {
        let unmounted = device(DeviceState::Unmounted, true);
        assert_eq!(
            actions_for(&unmounted, true),
            DeviceActions {
                mount: true,
                unmount: false,
                eject: true
            }
        );
        assert_eq!(
            actions_for(&mounted(true), true),
            DeviceActions {
                mount: false,
                unmount: true,
                eject: true
            }
        );
        let fstab = device(DeviceState::Mounted(p("/mnt/data")), false);
        assert_eq!(
            actions_for(&fstab, true),
            DeviceActions {
                mount: false,
                unmount: false,
                eject: false
            },
            "an internal mount outside the media roots is not the session's"
        );
        let locked = device(DeviceState::Locked, true);
        assert_eq!(
            actions_for(&locked, true),
            DeviceActions {
                mount: false,
                unmount: false,
                eject: true
            }
        );
        for device in [unmounted, mounted(true), locked] {
            assert_eq!(actions_for(&device, false), DeviceActions::default());
        }
    }

    #[test]
    fn an_unmounted_row_offers_mount_eject_and_copy_path() {
        let rows = device_menu_rows(DeviceActions {
            mount: true,
            unmount: false,
            eject: true,
        });
        assert_eq!(labels(&rows), ["Mount", "Eject", "-", "Copy path"]);
        let internal = device_menu_rows(DeviceActions {
            mount: true,
            unmount: false,
            eject: false,
        });
        assert_eq!(labels(&internal), ["Mount", "-", "Copy path"]);
        let locked = device_menu_rows(DeviceActions {
            mount: false,
            unmount: false,
            eject: true,
        });
        assert_eq!(labels(&locked), ["Eject", "-", "Copy path"]);
        let none = device_menu_rows(DeviceActions::default());
        assert_eq!(labels(&none), ["Copy path"]);
        assert!(
            rows.iter()
                .all(|row| !matches!(row, MenuRow::Item(item) if item.danger || !item.enabled))
        );
    }

    #[test]
    fn a_mounted_row_gets_unmount_and_eject_after_the_open_rows() {
        let base = vec![
            MenuRow::Item(MenuItemSpec::new(MenuCommand::Open, "Open")),
            MenuRow::Item(MenuItemSpec::new(
                MenuCommand::OpenInNewTab,
                "Open in new tab",
            )),
            MenuRow::Separator,
            MenuRow::Item(MenuItemSpec::new(
                MenuCommand::AddFavorite,
                "Add to favorites",
            )),
            MenuRow::Item(MenuItemSpec::new(MenuCommand::CopyPath, "Copy path")),
        ];
        let both = with_device_rows(
            base.clone(),
            DeviceActions {
                mount: false,
                unmount: true,
                eject: true,
            },
        );
        assert_eq!(
            labels(&both),
            [
                "Open",
                "Open in new tab",
                "-",
                "Unmount",
                "Eject",
                "-",
                "Add to favorites",
                "Copy path"
            ]
        );
        let only_unmount = with_device_rows(
            base.clone(),
            DeviceActions {
                mount: false,
                unmount: true,
                eject: false,
            },
        );
        assert_eq!(
            labels(&only_unmount),
            [
                "Open",
                "Open in new tab",
                "-",
                "Unmount",
                "-",
                "Add to favorites",
                "Copy path"
            ]
        );
        assert_eq!(
            with_device_rows(base.clone(), DeviceActions::default()),
            base
        );
    }

    #[test]
    fn the_muted_line_says_what_the_device_is_doing() {
        assert_eq!(
            caption(&device(DeviceState::Unmounted, true), None),
            "Not mounted"
        );
        assert_eq!(caption(&device(DeviceState::Locked, true), None), "Locked");
        assert_eq!(
            caption(
                &device(DeviceState::Unmounted, true),
                Some("Mounting\u{2026}")
            ),
            "Mounting\u{2026}"
        );
    }

    #[test]
    fn a_mounted_device_the_mount_table_missed_gets_a_volume_of_its_own() {
        let listed = Volume {
            label: "KINGSTON".to_owned(),
            path: p("/run/media/u/KINGSTON"),
            removable: true,
            usage: None,
            kind: VolumeKind::Disk,
        };
        let other = DeviceState::Mounted(p("/mnt/odd"));
        let devices = [
            mounted(true),
            device(other, true),
            device(DeviceState::Unmounted, true),
            device(DeviceState::Locked, true),
        ];
        assert_eq!(
            unlisted_volumes(std::slice::from_ref(&listed), &devices),
            [Volume {
                label: "KINGSTON".to_owned(),
                path: p("/mnt/odd"),
                removable: true,
                usage: None,
                kind: VolumeKind::Disk,
            }]
        );
        assert!(unlisted_volumes(&[], &[]).is_empty());
    }

    // --- running operations ---------------------------------------------------

    fn names(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn every_job_reports_the_paths_it_touches() {
        let trash = p("/home/u/.local/share/Trash");
        assert_eq!(
            job_paths(
                &Job::Create {
                    dir: p("/mnt/a"),
                    kind: NewKind::Folder
                },
                &trash
            ),
            [p("/mnt/a")]
        );
        assert_eq!(
            job_paths(
                &Job::Rename {
                    path: p("/mnt/a/x"),
                    new_name: OsString::from("y")
                },
                &trash
            ),
            [p("/mnt/a/x")]
        );
        assert_eq!(
            job_paths(&Job::Trash(vec![p("/mnt/a/x"), p("/home/u/y")]), &trash),
            [p("/mnt/a/x"), p("/home/u/y")]
        );
        assert_eq!(
            job_paths(&Job::DeletePermanently(vec![p("/mnt/a/x")]), &trash),
            [p("/mnt/a/x")]
        );
        for job in [
            Job::Restore(names(&["a"])),
            Job::DeleteFromTrash(names(&["a"])),
            Job::EmptyTrash(names(&["a"])),
        ] {
            assert_eq!(job_paths(&job, &trash), std::slice::from_ref(&trash));
        }
    }

    #[test]
    fn a_paste_touches_its_sources_and_its_destination() {
        let (requests, _) = channel();
        let (_, replies) = channel();
        let job = Job::Paste(PasteJob {
            sources: vec![p("/home/u/a"), p("/home/u/b")],
            dest: p("/run/media/u/USB/dir"),
            mode: ClipMode::Copy,
            link: ConflictLink { requests, replies },
        });
        assert_eq!(
            job_paths(&job, Path::new("/t")),
            [p("/home/u/a"), p("/home/u/b"), p("/run/media/u/USB/dir")]
        );
    }

    #[test]
    fn an_undo_touches_where_it_puts_things_back() {
        let identity = Identity {
            dev: 1,
            ino: 2,
            born: None,
        };
        let rename = UndoAction::Rename {
            old: p("/mnt/a/old"),
            new: p("/mnt/a/new"),
            identity,
        };
        assert_eq!(
            job_paths(&Job::Undo(rename), Path::new("/t")),
            [p("/mnt/a/old"), p("/mnt/a/new")]
        );
        let create = UndoAction::Create {
            path: p("/mnt/a/n"),
            kind: NewKind::File,
            identity,
        };
        assert_eq!(
            job_paths(&Job::Undo(create), Path::new("/t")),
            [p("/mnt/a/n")]
        );
        let placed = |from: &str, to: &str| crate::explorer::ops::PlacedEntry {
            from: p(from),
            to: p(to),
            identity,
        };
        let copy = UndoAction::Copy {
            items: vec![placed("/h/a", "/mnt/a/a")],
        };
        assert_eq!(
            job_paths(&Job::Undo(copy), Path::new("/t")),
            [p("/h/a"), p("/mnt/a/a")]
        );
        let moved = UndoAction::Move {
            items: vec![placed("/h/b", "/mnt/a/b")],
        };
        assert_eq!(
            job_paths(&Job::Undo(moved), Path::new("/t")),
            [p("/h/b"), p("/mnt/a/b")]
        );
    }

    #[test]
    fn only_operations_on_the_device_are_busy() {
        let running = vec![
            (1, vec![p("/home/u/a")]),
            (2, vec![p("/home/u/b"), p("/run/media/u/USB/x")]),
            (3, vec![p("/run/media/u/USB2/y")]),
            (4, vec![p("/run/media/u/USB")]),
        ];
        let mounts = [p("/run/media/u/USB")];
        assert_eq!(busy_ops(&running, &mounts), [2, 4]);
        assert_eq!(busy_ops(&running, &[]), Vec::<u64>::new());
        assert_eq!(busy_ops(&[], &mounts), Vec::<u64>::new());
    }

    // --- tabs -----------------------------------------------------------------

    #[test]
    fn a_paste_through_a_symlink_into_the_device_is_busy() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = std::env::temp_dir().join(format!("ff-busy-link-{}", std::process::id()));
        let mount = root.join("media/USB");
        std::fs::create_dir_all(&mount)?;
        let link = root.join("usblink");
        std::os::unix::fs::symlink(&mount, &link)?;
        let busy = busy_ops(
            &[(7, vec![p("/home/u/src"), link.clone()])],
            std::slice::from_ref(&mount),
        );
        let tabs = tabs_to_move(
            &[(1, Some(link.clone()), None)],
            std::slice::from_ref(&mount),
        );
        std::fs::remove_file(&link)?;
        std::fs::remove_dir(&mount)?;
        assert_eq!(
            busy,
            [7],
            "a copy into a symlink to the mount writes on the device"
        );
        assert_eq!(
            tabs.len(),
            1,
            "a tab shown through a symlink must leave the device"
        );
        Ok(())
    }

    #[test]
    fn tabs_on_the_device_move_including_one_still_opening_it() {
        let mounts = [p("/run/media/u/USB")];
        let tabs = vec![
            (0, Some(p("/home/u")), None),
            (1, Some(p("/run/media/u/USB/photos")), None),
            (2, Some(p("/home/u")), Some(p("/run/media/u/USB/docs"))),
            (3, Some(p("/run/media/u/USB2")), None),
            (4, None, None),
        ];
        assert_eq!(
            tabs_to_move(&tabs, &mounts),
            [
                (1, p("/run/media/u/USB/photos")),
                (2, p("/run/media/u/USB/docs"))
            ]
        );
        assert!(tabs_to_move(&tabs, &[]).is_empty());
    }

    #[test]
    fn the_moved_notice_names_the_device_and_the_new_folder() {
        assert_eq!(
            moved_notice("KINGSTON", Path::new("/run/media/u")),
            "KINGSTON was unmounted; showing /run/media/u"
        );
    }

    // --- draining: new work on a device that is going away ----------------------

    #[test]
    fn work_on_a_device_being_ejected_is_refused_by_name() {
        let eject = plan("KINGSTON", "/run/media/u/USB");
        let unmount = DevicePlan {
            power_off: None,
            ..plan("DATA", "/run/media/u/DATA")
        };
        let draining = [eject, unmount];
        // Reading from it, writing into it, creating in it: all touched paths.
        assert_eq!(
            refusal(&draining, &[p("/home/u/a"), p("/run/media/u/USB/dir")]),
            Some("KINGSTON is being ejected".to_owned())
        );
        assert_eq!(
            refusal(&draining, &[p("/run/media/u/DATA")]),
            Some("DATA is being unmounted".to_owned())
        );
        assert_eq!(
            refusal(&draining, &[p("/run/media/u/USB2/x"), p("/home/u")]),
            None
        );
        assert_eq!(refusal(&[], &[p("/run/media/u/USB/dir")]), None);
        // Every mount point of the device counts, not only the first.
        let two = DevicePlan {
            unmount: vec![UnmountStep {
                device: p("/dev/sdb1"),
                mounts: vec![p("/run/media/u/A"), p("/mnt/bind")],
                via: UnmountVia::Udisks,
            }],
            ..plan("TWO", "/unused")
        };
        assert_eq!(
            refusal(std::slice::from_ref(&two), &[p("/mnt/bind/x")]),
            Some("TWO is being ejected".to_owned())
        );
    }

    #[test]
    fn the_hub_drains_what_is_queued_and_what_is_running() {
        let mut hub = DeviceHub::default();
        assert!(!hub.is_draining());
        hub.pending.push(plan("A", "/m/a"));
        assert!(hub.is_draining());
        hub.pending.clear();
        hub.active.push(plan("B", "/m/b"));
        assert!(hub.is_draining());
        hub.pending.push(plan("A", "/m/a"));
        let labels: Vec<&str> = hub.draining().map(|plan| plan.label.as_str()).collect();
        assert_eq!(labels, ["A", "B"]);
    }

    #[test]
    fn the_plan_stops_before_unmounting_when_an_operation_started_using_the_device() {
        let eject = plan("KINGSTON", "/run/media/u/USB");
        let none: Vec<(u64, Vec<PathBuf>)> = vec![(1, vec![p("/home/u/a")])];
        assert_eq!(recheck(&none, &eject), Ok(()));
        assert_eq!(recheck(&[], &eject), Ok(()));
        let one = vec![(2, vec![p("/home/u/a"), p("/run/media/u/USB/x")])];
        assert_eq!(
            recheck(&one, &eject),
            Err("an operation started using it".to_owned())
        );
        let two = vec![
            (2, vec![p("/run/media/u/USB/x")]),
            (3, vec![p("/run/media/u/USB")]),
        ];
        assert_eq!(
            recheck(&two, &eject),
            Err("2 operations started using it".to_owned())
        );
    }

    #[test]
    fn removed_safely_only_after_every_step_including_the_power_off() {
        let eject = plan("KINGSTON", "/run/media/u/USB");
        assert_eq!(
            plan_notice(&eject, &Ok(())),
            (
                Severity::Success,
                "KINGSTON can be removed safely".to_owned()
            )
        );
        let failed = DeviceError::Failed {
            reason: "target is busy".to_owned(),
        };
        let (severity, text) = plan_notice(&eject, &Err(failed.clone()));
        assert_eq!(severity, Severity::Error);
        assert_eq!(text, "Could not eject KINGSTON: target is busy");
        assert!(!text.contains("safely"));
        let unmount = DevicePlan {
            power_off: None,
            ..eject
        };
        assert_eq!(
            plan_notice(&unmount, &Ok(())),
            (Severity::Success, "Unmounted KINGSTON".to_owned())
        );
        assert_eq!(
            plan_notice(&unmount, &Err(failed)).1,
            "Could not unmount KINGSTON: target is busy"
        );
    }

    #[test]
    fn tabs_that_left_a_device_that_stayed_mounted_say_so() {
        assert_eq!(
            failed_notice("KINGSTON", "target is busy"),
            "Could not unmount KINGSTON; target is busy"
        );
        assert!(!failed_notice("K", "x").contains("was unmounted"));
        assert_eq!(leaving_notice("KINGSTON"), "Unmounting KINGSTON\u{2026}");
    }

    #[test]
    fn wait_and_eject_never_promises_zero_operations() {
        let eject = plan("KINGSTON", "/run/media/u/USB");
        assert_eq!(queued_notice(&eject, 0), None);
        assert_eq!(
            queued_notice(&eject, 1),
            Some("Will eject KINGSTON when 1 operation finishes".to_owned())
        );
        assert_eq!(
            queued_notice(&eject, 3),
            Some("Will eject KINGSTON when 3 operations finish".to_owned())
        );
    }

    #[test]
    fn tab_moves_name_where_each_tab_goes_past_a_symlink() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = std::env::temp_dir().join(format!("ff-tab-link-{}", std::process::id()));
        let mount = root.join("media/USB");
        std::fs::create_dir_all(mount.join("sub"))?;
        let link = root.join("usblink");
        std::os::unix::fs::symlink(&mount, &link)?;
        let tabs = vec![
            (1, Some(link.join("sub")), None),
            (2, Some(root.clone()), None),
        ];
        let moves = plan_tab_moves(&tabs, std::slice::from_ref(&mount), None);
        std::fs::remove_file(&link)?;
        std::fs::remove_dir_all(&root)?;
        assert_eq!(moves, [(1, root.join("usblink/sub"), root)]);
        Ok(())
    }

    #[test]
    fn back_forward_and_up_into_a_symlinked_folder_are_refused_by_real_location()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::explorer::state::{ExplorerState, Transition};
        let root = std::env::temp_dir().join(format!("ff-nav-link-{}", std::process::id()));
        let mount = root.join("media/USB");
        std::fs::create_dir_all(mount.join("existing"))?;
        std::fs::create_dir_all(root.join("home"))?;
        let link = root.join("home/usblink");
        std::os::unix::fs::symlink(&mount, &link)?;
        let draining = plan("USB", &mount.to_string_lossy());

        let dir = |path: &Path| Location::Directory(path.to_path_buf());
        let mut state = ExplorerState::new(dir(&root.join("home")), Some(root.join("home")));
        for step in [link.join("existing"), root.join("home")] {
            let scan = state.begin(Transition::Visit(dir(&step)));
            if let Some(scan) = scan {
                state.commit(scan, Default::default());
            }
        }
        // Back lands in the symlinked folder; Up from it would land in the link.
        let back = state.target_of(&Transition::Back);
        let Some(Location::Directory(back)) = back else {
            return Err("back has a target".into());
        };
        assert_eq!(back, link.join("existing"));
        assert_eq!(
            refusal(std::slice::from_ref(&draining), std::slice::from_ref(&back)),
            None,
            "by text the symlinked folder looks harmless"
        );
        let real = real_location(&back);
        let verdict = refusal(std::slice::from_ref(&draining), &[real]);
        let cleanup = std::fs::remove_dir_all(&root);
        assert_eq!(verdict, Some("USB is being ejected".to_owned()));
        cleanup?;
        Ok(())
    }

    #[test]
    fn a_deferred_step_enters_only_the_folder_that_was_checked()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::explorer::state::ExplorerState;
        let root = std::env::temp_dir().join(format!("ff-checked-step-{}", std::process::id()));
        let mount = root.join("media/USB");
        std::fs::create_dir_all(mount.join("existing"))?;
        std::fs::create_dir_all(root.join("home/safe"))?;
        let link = root.join("home/usblink");
        std::os::unix::fs::symlink(&mount, &link)?;
        let draining = plan("USB", &mount.to_string_lossy());
        let dir = |path: &Path| Location::Directory(path.to_path_buf());
        let visit = |state: &mut ExplorerState, path: &Path| {
            if let Some(scan) = state.begin(Transition::Visit(dir(path))) {
                state.commit(scan, Default::default());
            }
        };
        // History: the symlinked folder, then a safe one, then where we are.
        let safe = root.join("home/safe");
        let mut state = ExplorerState::new(dir(&link), Some(root.join("home")));
        visit(&mut state, &safe);
        visit(&mut state, &root.join("home"));

        // Two quick Back presses: nothing has committed yet, so both check
        // the same target, the safe folder.
        let first = state.target_of(&Transition::Back);
        let second = state.target_of(&Transition::Back);
        assert_eq!(first, Some(dir(&safe)));
        assert_eq!(second, first);
        let (Some(first), Some(second)) = (first, second) else {
            return Err("back has a target".into());
        };

        // The first deferred step commits; the second finds the history one
        // step further, on the symlink into the draining device.
        let step = CheckedStep::of(Transition::Back, state.target_of(&Transition::Back), first);
        assert_eq!(step.into_transition(), Transition::Back, "as asked");
        let Some(scan) = state.begin(Transition::Back) else {
            return Err("the first Back starts a scan".into());
        };
        state.commit(scan, Default::default());
        let now = state.target_of(&Transition::Back);
        assert_eq!(now, Some(dir(&link)), "the unchecked entry is next");
        assert_eq!(
            refusal(std::slice::from_ref(&draining), &[real_location(&link)]),
            Some("USB is being ejected".to_owned()),
            "entering it would have been refused had it been checked"
        );

        let step = CheckedStep::of(Transition::Back, now, second);
        let entered = state
            .begin(step.into_transition())
            .map(|scan| scan.location);
        let cleanup = std::fs::remove_dir_all(&root);
        assert_eq!(
            entered,
            Some(dir(&safe)),
            "the checked folder, not the link"
        );
        cleanup?;
        Ok(())
    }

    /// Runs `transition` to its end on `state`, as a finished device check
    /// followed by a finished scan would.
    fn settle_step(state: &mut crate::explorer::state::ExplorerState, step: Transition) {
        if let Some(scan) = state.begin(step) {
            state.commit(scan, Default::default());
        }
    }

    #[test]
    fn a_double_up_while_draining_stays_on_the_checked_parent()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::explorer::state::ExplorerState;
        let dir = |path: &str| Location::Directory(PathBuf::from(path));
        let mut state = ExplorerState::new(dir("/m/a/b/c"), Some(PathBuf::from("/m")));

        // Two quick Up presses: nothing has committed, both check /m/a/b.
        let first = state.target_of(&Transition::Up);
        let second = state.target_of(&Transition::Up);
        assert_eq!(first, Some(dir("/m/a/b")));
        assert_eq!(second, first);
        let (Some(first), Some(second)) = (first, second) else {
            return Err("up has a target".into());
        };

        let step = CheckedStep::of(Transition::Up, state.target_of(&Transition::Up), first);
        assert_eq!(step.into_transition(), Transition::Up, "as asked");
        settle_step(&mut state, Transition::Up);
        let now = state.target_of(&Transition::Up);
        assert_eq!(now, Some(dir("/m/a")), "the unchecked parent is next");

        let step = CheckedStep::of(Transition::Up, now, second);
        let entered = state
            .begin(step.into_transition())
            .map(|scan| scan.location);
        assert_eq!(entered, Some(dir("/m/a/b")), "the checked parent, not /m/a");
        Ok(())
    }

    #[test]
    fn a_double_forward_never_reaches_the_unchecked_entry() -> Result<(), Box<dyn std::error::Error>>
    {
        use crate::explorer::state::ExplorerState;
        let dir = |path: &str| Location::Directory(PathBuf::from(path));
        let mut state = ExplorerState::new(dir("/m/start"), Some(PathBuf::from("/m")));
        settle_step(&mut state, Transition::Visit(dir("/m/one")));
        settle_step(&mut state, Transition::Visit(dir("/m/two")));
        settle_step(&mut state, Transition::Back);
        settle_step(&mut state, Transition::Back);
        assert_eq!(state.target_of(&Transition::Back), None, "at the start");

        // Two quick Forward presses: both check /m/one.
        let first = state.target_of(&Transition::Forward);
        let second = state.target_of(&Transition::Forward);
        assert_eq!(first, Some(dir("/m/one")));
        assert_eq!(second, first);
        let (Some(first), Some(second)) = (first, second) else {
            return Err("forward has a target".into());
        };

        let step = CheckedStep::of(
            Transition::Forward,
            state.target_of(&Transition::Forward),
            first,
        );
        assert_eq!(step.into_transition(), Transition::Forward, "as asked");
        settle_step(&mut state, Transition::Forward);
        let now = state.target_of(&Transition::Forward);
        assert_eq!(now, Some(dir("/m/two")), "the unchecked entry is next");

        let step = CheckedStep::of(Transition::Forward, now, second);
        let entered = state
            .begin(step.into_transition())
            .map(|scan| scan.location);
        assert_eq!(
            entered,
            Some(dir("/m/one")),
            "the checked entry, not /m/two"
        );
        Ok(())
    }

    // --- polling and queue ----------------------------------------------------

    #[test]
    fn the_device_list_is_read_at_once_then_only_when_events_settle() {
        let now = Instant::now();
        let ago = |millis: u64| now.checked_sub(Duration::from_millis(millis));
        let idle = RefreshGate::default();
        assert!(
            refresh_due(&idle, None, now, false, false),
            "the first read does not wait for anything"
        );
        assert!(
            !refresh_due(&idle, None, now, false, true),
            "never two at once"
        );
        // No polling while the watches work, focused or not, however long.
        for last in [ago(1_000), ago(60_000), ago(3_600_000)] {
            assert!(!refresh_due(&idle, last, now, false, false), "{last:?}");
        }
        // An event is read once it settled (300 ms), not before.
        let mut gate = RefreshGate::default();
        gate.note(now - Duration::from_millis(100), DeviceSource::Gvfs);
        assert!(
            !refresh_due(&gate, ago(5_000), now, false, false),
            "still bursting"
        );
        let mut gate = RefreshGate::default();
        gate.note(now - Duration::from_millis(400), DeviceSource::Gvfs);
        assert!(refresh_due(&gate, ago(5_000), now, false, false), "settled");
        assert!(
            !refresh_due(&gate, ago(5_000), now, false, true),
            "an event during a read waits for it to end"
        );
    }

    #[test]
    fn the_slow_poll_runs_only_when_a_watch_is_missing() {
        let now = Instant::now();
        let ago = |seconds: u64| now.checked_sub(Duration::from_secs(seconds));
        let idle = RefreshGate::default();
        assert!(!refresh_due(&idle, ago(29), now, true, false));
        assert!(
            refresh_due(&idle, ago(30), now, true, false),
            "30 s fallback"
        );
        assert!(!refresh_due(&idle, ago(30), now, true, true));
        assert!(
            !refresh_due(&idle, ago(3_600), now, false, false),
            "watches work"
        );
        let _ = safety_poll_due;
    }

    #[test]
    fn watcher_events_feed_the_gate_and_a_stopped_watcher_is_degraded() {
        let now = Instant::now();
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut hub = DeviceHub {
            signals: Some(receiver),
            ..DeviceHub::default()
        };
        hub.collect_signals(now);
        assert!(!hub.gate.pending(), "nothing reported");
        sender.send(DeviceSource::Gvfs).ok();
        sender.send(DeviceSource::Mounts).ok();
        hub.collect_signals(now);
        assert!(hub.gate.pending());
        assert!(!hub.gate.ready(now + Duration::from_millis(299)));
        assert!(hub.gate.ready(now + Duration::from_millis(300)));
        assert!(!hub.signals_closed);
        drop(sender);
        hub.collect_signals(now);
        assert!(hub.signals_closed, "the thread is gone");
        assert!(hub.degraded());
        assert!(DeviceHub::default().degraded(), "no watcher at all");
    }

    #[test]
    fn regaining_the_focus_looks_at_the_devices_again_once() {
        let now = Instant::now();
        let mut hub = DeviceHub {
            was_active: true,
            ..DeviceHub::default()
        };
        hub.note_focus(true, now);
        hub.note_focus(false, now);
        assert!(!hub.gate.pending(), "losing the focus is not news");
        hub.note_focus(true, now);
        assert!(hub.gate.pending(), "the focus came back");
        hub.gate.start(now);
        hub.note_focus(true, now);
        assert!(!hub.gate.pending(), "staying focused is not news");
    }

    // --- gvfs mounts ----------------------------------------------------------

    fn gvfs_volume(kind: VolumeKind) -> Volume {
        Volume {
            label: "Google Pixel 8".to_owned(),
            path: p("/run/user/1000/gvfs/mtp:host=Google_Pixel_8_39060DLJH0031N"),
            removable: false,
            usage: None,
            kind,
        }
    }

    #[test]
    fn a_gvfs_mount_offers_unmount_only_and_only_with_gio() {
        for kind in [VolumeKind::Phone, VolumeKind::Share, VolumeKind::Remote] {
            assert_eq!(
                gvfs_actions(kind, true),
                DeviceActions {
                    mount: false,
                    unmount: true,
                    eject: false
                },
                "{kind:?}"
            );
            assert_eq!(
                gvfs_actions(kind, false),
                DeviceActions::default(),
                "{kind:?}"
            );
        }
        assert_eq!(
            gvfs_actions(VolumeKind::Disk, true),
            DeviceActions::default()
        );
    }

    #[test]
    fn unmounting_a_gvfs_mount_runs_gio_on_its_path() {
        let phone = gvfs_volume(VolumeKind::Phone);
        let disk = Volume {
            kind: VolumeKind::Disk,
            path: p("/mnt/data"),
            ..gvfs_volume(VolumeKind::Disk)
        };
        let volumes = [phone.clone(), disk.clone()];
        let gio = Path::new("/usr/bin/gio");

        let plan = gvfs_unmount_plan(&volumes, &phone.path, Some(gio));

        let plan = plan.map(|plan| (plan.label.clone(), plan.mounts(), plan.power_off.clone()));
        assert_eq!(
            plan,
            Some(("Google Pixel 8".to_owned(), vec![phone.path.clone()], None))
        );
        assert!(
            gvfs_unmount_plan(&volumes, &phone.path, None).is_none(),
            "no gio"
        );
        assert!(
            gvfs_unmount_plan(&volumes, &disk.path, Some(gio)).is_none(),
            "a disk"
        );
        assert!(gvfs_unmount_plan(&volumes, Path::new("/nowhere"), Some(gio)).is_none());
    }

    #[test]
    fn a_plan_queues_once_and_starts_in_order_when_idle() {
        let mut pending = Vec::new();
        assert!(queue_plan(&mut pending, plan("A", "/m/a")));
        assert!(
            !queue_plan(&mut pending, plan("A", "/m/a")),
            "the same eject is not queued twice"
        );
        assert!(queue_plan(&mut pending, plan("B", "/m/b")));
        assert!(queue_plan(&mut pending, plan("C", "/m/c")));
        let ready = take_ready(&mut pending, |mounts| mounts == [p("/m/b")]);
        assert_eq!(
            ready.iter().map(|p| p.label.as_str()).collect::<Vec<_>>(),
            ["A", "C"]
        );
        assert_eq!(pending.len(), 1, "B still waits");
        let ready = take_ready(&mut pending, |_| false);
        assert_eq!(ready.len(), 1);
        assert!(pending.is_empty());
        assert!(take_ready(&mut pending, |_| false).is_empty());
    }

    // --- the busy dialog ------------------------------------------------------

    #[test]
    fn the_busy_dialog_names_the_device_and_how_many_operations_use_it() {
        use crate::explorer::ops::{DialogKind, dialog_text};
        let titles = vec![
            "Copying 3 items to USB".to_owned(),
            "Moving a.iso".to_owned(),
        ];
        let eject = dialog_text(
            &DialogKind::DeviceBusy {
                label: "KINGSTON".to_owned(),
                count: 2,
                eject: true,
            },
            &titles,
        );
        assert_eq!(eject.title, "2 operations are using KINGSTON");
        assert_eq!(
            eject.body,
            "Ejecting now could corrupt data. Wait for them to finish, or cancel."
        );
        assert_eq!(eject.confirm, "Wait and eject");
        assert_eq!(eject.names, titles);
        let one = dialog_text(
            &DialogKind::DeviceBusy {
                label: "KINGSTON".to_owned(),
                count: 1,
                eject: false,
            },
            titles.get(..1).unwrap_or_default(),
        );
        assert_eq!(one.title, "1 operation is using KINGSTON");
        assert_eq!(
            one.body,
            "Unmounting now could corrupt data. Wait for it to finish, or cancel."
        );
        assert_eq!(one.confirm, "Wait and unmount");
    }

    // --- missing tools --------------------------------------------------------

    #[test]
    fn a_missing_tool_gets_a_one_line_notice() {
        let both = DeviceHub {
            udisksctl: Some(p("/bin/udisksctl")),
            lsblk: Some(p("/bin/lsblk")),
            ..DeviceHub::default()
        };
        assert_eq!(both.notice(), None);
        let no_udisks = DeviceHub {
            udisksctl: None,
            ..both_clone(&both)
        };
        assert_eq!(
            no_udisks.notice(),
            Some("udisksctl was not found: mounting and ejecting are unavailable")
        );
        let no_lsblk = DeviceHub {
            lsblk: None,
            ..both_clone(&both)
        };
        assert_eq!(
            no_lsblk.notice(),
            Some("lsblk was not found: devices that are not mounted are not listed")
        );
    }

    fn both_clone(hub: &DeviceHub) -> DeviceHub {
        DeviceHub {
            udisksctl: hub.udisksctl.clone(),
            lsblk: hub.lsblk.clone(),
            ..DeviceHub::default()
        }
    }

    #[test]
    fn the_row_word_follows_the_device_or_its_disk() {
        let mut hub = DeviceHub::default();
        let device = device(DeviceState::Unmounted, true);
        assert_eq!(hub.working_on(&device), None);
        hub.working.insert(p("/dev/sdb"), "Ejecting\u{2026}");
        assert_eq!(hub.working_on(&device), Some("Ejecting\u{2026}"));
        hub.working.insert(p("/dev/sdb1"), "Mounting\u{2026}");
        assert_eq!(hub.working_on(&device), Some("Mounting\u{2026}"));
    }
}
