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
use std::time::{Duration, Instant};

use fileflux::core::Location;
use fileflux::core::devices::{
    BlockNode, Device, DeviceError, DevicePlan, DeviceState, devices, eject_plan, fallback_target,
    find_tool, list_blocks, mount, path_uses_mounts, run_plan, unmount_plan,
};
use fileflux::core::places::{Volume, discover_volumes};
use gpui::{ClipboardItem, Context, Window};

use super::ops::{
    DialogAction, DialogKind, Job, MenuCommand, MenuItemSpec, MenuRow, Severity, UndoAction,
};
use super::state::Transition;
use super::view::{Explorer, TabId};

/// How often the device list is re-read while the window is focused.
///
/// A poll (the mount table plus `lsblk`, one short process, off the UI
/// thread, never two at once, only while the window has the focus) was
/// chosen over watching `/dev/disk/by-uuid`: that directory does not change
/// when a device is mounted or unmounted, nor for filesystems without a UUID
/// or when another tool does the mounting, and a USB stick plugged in
/// while the window is in the background is picked up on the next focus.
pub(crate) const POLL_EVERY: Duration = Duration::from_secs(2);

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
            })
        })
        .collect()
}

/// The ids of the running operations (id, paths they touch) that use one
/// of `mounts`.
pub(crate) fn busy_ops(running: &[(u64, Vec<PathBuf>)], mounts: &[PathBuf]) -> Vec<u64> {
    running
        .iter()
        .filter(|(_, paths)| paths.iter().any(|path| path_uses_mounts(path, mounts)))
        .map(|(id, _)| *id)
        .collect()
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
                .find(|path| path_uses_mounts(path, mounts))
                .map(|path| (*id, path.clone()))
        })
        .collect()
}

/// The notice a tab shows after it left an unmounted device.
pub(crate) fn moved_notice(label: &str, target: &Path) -> String {
    format!("{label} was unmounted; showing {}", target.display())
}

/// Whether the device list is re-read now: never while a read is running,
/// at once the first time, and then every [`POLL_EVERY`] while the window
/// has the focus.
pub(crate) fn poll_due(
    last: Option<Instant>,
    now: Instant,
    window_active: bool,
    in_flight: bool,
) -> bool {
    if in_flight {
        return false;
    }
    match last {
        None => true,
        Some(last) => window_active && now.saturating_duration_since(last) >= POLL_EVERY,
    }
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
    pub(crate) last_poll: Option<Instant>,
    pub(crate) polling: bool,
}

impl DeviceHub {
    /// Looks for `udisksctl` and `lsblk` on `$PATH`.
    pub(crate) fn from_env() -> Self {
        let path = std::env::var_os("PATH");
        Self {
            udisksctl: find_tool("udisksctl", path.as_deref()),
            lsblk: find_tool("lsblk", path.as_deref()),
            ..Self::default()
        }
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
        Location::Trash => None,
    }
}

impl Explorer {
    /// What the device with block node `path` offers right now.
    pub(crate) fn device_actions(&self, path: &Path) -> Option<DeviceActions> {
        let device = self.devices.find(path)?;
        Some(actions_for(device, self.devices.udisksctl.is_some()))
    }

    /// Runs on every pump tick: re-reads the devices when due and starts the
    /// queued unmounts whose operations have ended.
    pub(crate) fn pump_devices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.places.is_some()
            && poll_due(
                self.devices.last_poll,
                Instant::now(),
                window.is_window_active(),
                self.devices.polling,
            )
        {
            self.refresh_devices(cx);
        }
        self.start_ready_plans(window, cx);
    }

    /// Reads the mount table and `lsblk` on the background executor and
    /// applies what changed.
    fn refresh_devices(&mut self, cx: &mut Context<Self>) {
        self.devices.polling = true;
        self.devices.last_poll = Some(Instant::now());
        let home = self.home.clone();
        let lsblk = self.devices.lsblk.clone();
        let read = cx.background_executor().spawn(async move {
            let volumes = discover_volumes(home.as_deref());
            let blocks = lsblk.map(|program| list_blocks(&program));
            (volumes, blocks)
        });
        cx.spawn(async move |this, cx| {
            let (volumes, blocks) = read.await;
            if let Err(error) =
                this.update(cx, |this, cx| this.apply_device_read(volumes, blocks, cx))
            {
                log::debug!("the explorer closed before the devices were read: {error}");
            }
        })
        .detach();
    }

    fn apply_device_read(
        &mut self,
        volumes: io::Result<Vec<Volume>>,
        blocks: Option<Result<Vec<BlockNode>, DeviceError>>,
        cx: &mut Context<Self>,
    ) {
        self.devices.polling = false;
        let mut changed = false;
        match volumes {
            Ok(volumes) => {
                if let Some(places) = self.places.as_mut()
                    && places.volumes != volumes
                {
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
            // every two seconds.
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
        let plan = self.devices.find(path).and_then(unmount_plan);
        match plan {
            Some(plan) => self.request_device_plan(plan, window, cx),
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
    fn running_touches(&self) -> Vec<(u64, Vec<PathBuf>)> {
        self.op_contexts
            .iter()
            .map(|(id, context)| (*id, context.touches.clone()))
            .collect()
    }

    /// Unmounting while an operation writes there can corrupt data: asks
    /// first when any uses the device, else goes ahead.
    fn request_device_plan(
        &mut self,
        plan: DevicePlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let busy = busy_ops(&self.running_touches(), &plan.mounts());
        if busy.is_empty() {
            return self.run_device_plan(plan, window, cx);
        }
        let titles: Vec<String> = busy
            .iter()
            .filter_map(|id| self.progress.title(*id).map(str::to_owned))
            .collect();
        self.open_dialog(
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
    }

    /// "Wait and eject": the plan starts once nothing uses the device.
    pub(crate) fn queue_device_plan(
        &mut self,
        plan: DevicePlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = busy_ops(&self.running_touches(), &plan.mounts()).len();
        let verb = if plan.power_off.is_some() {
            "eject"
        } else {
            "unmount"
        };
        let text = format!(
            "Will {verb} {} when {count} operation{} finish{}",
            plan.label,
            if count == 1 { "" } else { "s" },
            if count == 1 { "es" } else { "" },
        );
        if queue_plan(&mut self.devices.pending, plan) {
            self.say(Severity::Info, text, window, cx);
        }
    }

    fn start_ready_plans(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.devices.pending.is_empty() {
            return;
        }
        let running = self.running_touches();
        let ready = take_ready(&mut self.devices.pending, |mounts| {
            !busy_ops(&running, mounts).is_empty()
        });
        for plan in ready {
            self.run_device_plan(plan, window, cx);
        }
    }

    /// The tabs that show, or are opening, a folder under `mounts`.
    fn tabs_on(&self, mounts: &[PathBuf]) -> Vec<(TabId, PathBuf)> {
        let snapshot: Vec<_> = self
            .tabs
            .iter()
            .map(|tab| {
                (
                    tab.id,
                    directory_of(tab.state.current()),
                    tab.state.pending_directory().map(Path::to_path_buf),
                )
            })
            .collect();
        tabs_to_move(&snapshot, mounts)
    }

    /// Moves every tab off `mounts` to the nearest ancestor outside them,
    /// with the usual notice.
    fn move_tabs_off(
        &mut self,
        mounts: &[PathBuf],
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (id, leaving) in self.tabs_on(mounts) {
            let target = fallback_target(&leaving, mounts, self.home.as_deref(), Path::exists);
            let notice = moved_notice(label, &target);
            let scan = self
                .tabs
                .by_id_mut(id)
                .and_then(|tab| tab.state.begin_fallback(target, notice));
            self.start_scan_in(id, scan, window, cx);
        }
    }

    /// Runs the plan: the tabs leave and their watches are released first,
    /// then `udisksctl` runs off the UI thread.
    fn run_device_plan(&mut self, plan: DevicePlan, window: &mut Window, cx: &mut Context<Self>) {
        let Some(udisksctl) = self.devices.udisksctl.clone() else {
            return;
        };
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
        let mounts = plan.mounts();
        self.move_tabs_off(&mounts, &plan.label, window, cx);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            // The fallback scans end within a moment; an inotify watch on the
            // device would keep it busy, so wait for the tabs to be gone.
            for _ in 0..TAB_WAIT_STEPS {
                let clear = this
                    .update_in(cx, |this, _window, _cx| this.tabs_on(&mounts).is_empty())
                    .unwrap_or(true);
                if clear {
                    break;
                }
                cx.background_executor().timer(TAB_WAIT_STEP).await;
            }
            if this
                .update_in(cx, |this, _window, _cx| this.release_watches_under(&mounts))
                .is_err()
            {
                return;
            }
            let to_run = plan.clone();
            let result = cx
                .background_executor()
                .spawn(async move { run_plan(&udisksctl, &to_run) })
                .await;
            if let Err(error) = this.update_in(cx, |this, window, cx| {
                this.finish_device_plan(&plan, key.as_deref(), result, window, cx);
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = key {
            self.devices.working.remove(key);
        }
        // Show the new state at once instead of at the next poll.
        self.devices.last_poll = None;
        let eject = plan.power_off.is_some();
        match result {
            Ok(()) if eject => {
                let text = format!("{} can be removed safely", plan.label);
                self.say(Severity::Success, text, window, cx);
            }
            Ok(()) => self.say(
                Severity::Success,
                format!("Unmounted {}", plan.label),
                window,
                cx,
            ),
            Err(error) => {
                let verb = if eject { "eject" } else { "unmount" };
                let text = format!("Could not {verb} {}: {error}", plan.label);
                self.say(Severity::Error, text, window, cx);
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::ops::paste::{ClipMode, ConflictLink, PasteJob};
    use crate::explorer::ops::{Identity, NewKind, UndoAction};
    use fileflux::core::devices::{DeviceState, UnmountStep};
    use std::ffi::OsString;
    use std::sync::mpsc::channel;

    fn p(text: &str) -> PathBuf {
        PathBuf::from(text)
    }

    fn device(state: DeviceState, ejectable: bool) -> Device {
        Device {
            path: p("/dev/sdb1"),
            disk: p("/dev/sdb"),
            label: "KINGSTON".to_owned(),
            fstype: Some("vfat".to_owned()),
            size: 15_000_000_000,
            state,
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
                mount: p(mount),
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
        let identity = Identity { dev: 1, ino: 2 };
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

    // --- polling and queue ----------------------------------------------------

    #[test]
    fn the_device_list_is_read_first_at_once_then_every_two_seconds_while_focused() {
        let now = Instant::now();
        let ago = |seconds: u64| now.checked_sub(Duration::from_secs(seconds));
        assert!(
            poll_due(None, now, false, false),
            "the first read does not wait for focus"
        );
        assert!(!poll_due(None, now, true, true), "never two at once");
        assert!(!poll_due(ago(1), now, true, false), "too soon");
        assert!(poll_due(ago(2), now, true, false));
        assert!(poll_due(ago(60), now, true, false));
        assert!(
            !poll_due(ago(60), now, false, false),
            "an unfocused window does not poll"
        );
        assert!(!poll_due(ago(60), now, true, true));
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
