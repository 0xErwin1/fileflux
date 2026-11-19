//! Device guards that decide by where a path really is (symlinks, `..`,
//! destinations that do not exist yet) and the system-disk rules of the
//! device list. Every test works in a fresh directory under the system temp
//! directory and never touches a real device.

#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};

use fileflux::core::devices::{
    devices, eject_plan, fallback_target, parse_lsblk, path_is_on_mounts, real_location,
};

/// A scratch directory removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fileflux-real-location-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}

#[test]
fn dot_dot_and_missing_parts_under_a_symlink_count_as_the_device() {
    let scratch = Scratch::new("paths");
    let d = &scratch.0;
    let mnt = d.join("media/KINGSTON");
    std::fs::create_dir_all(mnt.join("sub")).expect("mount");
    std::fs::create_dir_all(d.join("home/a")).expect("home");
    let link = d.join("home/usblink");
    std::os::unix::fs::symlink(&mnt, &link).expect("link");
    let mounts = vec![mnt.clone()];

    // `..` that resolves into the mount.
    assert!(path_is_on_mounts(&d.join("home/a/../usblink/x"), &mounts));
    assert!(path_is_on_mounts(
        &d.join("home/a/../../media/KINGSTON/sub"),
        &mounts
    ));
    // A destination that does not exist yet, under a symlink.
    assert!(path_is_on_mounts(
        &link.join("new/deeper/file.txt"),
        &mounts
    ));
    // A missing folder, then `..` back into the mount.
    assert!(path_is_on_mounts(&link.join("nothere/../sub"), &mounts));
    // A sibling that shares a prefix, and `..` leaving the mount, are not.
    std::fs::create_dir_all(d.join("media/KINGSTON2")).expect("sibling");
    assert!(!path_is_on_mounts(&d.join("media/KINGSTON2/f"), &mounts));
    assert!(!path_is_on_mounts(&d.join("home/a"), &mounts));
    // The symlinked mount point itself in a plan.
    assert_eq!(
        real_location(&link.join("new")),
        std::fs::canonicalize(&mnt)
            .expect("canonical mount")
            .join("new")
    );
}

#[test]
fn a_symlink_loop_neither_hangs_nor_counts() {
    let scratch = Scratch::new("loop");
    let d = &scratch.0;
    let mnt = d.join("media/KINGSTON");
    std::fs::create_dir_all(&mnt).expect("mount");
    let (l1, l2) = (d.join("loop1"), d.join("loop2"));
    std::os::unix::fs::symlink(&l2, &l1).expect("loop1");
    std::os::unix::fs::symlink(&l1, &l2).expect("loop2");
    assert!(!path_is_on_mounts(&l1.join("x"), &[mnt]));
}

#[test]
fn a_history_entry_through_a_symlink_resolves_into_the_device() {
    // What Back, Forward and Up navigate to is checked by real location:
    // `~/usblink/existing` is on the device although its text is not.
    let scratch = Scratch::new("history");
    let d = &scratch.0;
    let mnt = d.join("media/USB");
    std::fs::create_dir_all(mnt.join("existing")).expect("mount");
    std::fs::create_dir_all(d.join("home")).expect("home");
    let link = d.join("home/usblink");
    std::os::unix::fs::symlink(&mnt, &link).expect("link");
    let mounts = vec![mnt];
    let entry = link.join("existing");
    assert!(!entry.starts_with(&mounts[0]), "by text it is not there");
    assert!(path_is_on_mounts(&entry, &mounts));
    assert!(path_is_on_mounts(&entry.join("gone"), &mounts));
}

#[test]
fn a_fallback_for_a_tab_shown_with_dot_dot_is_normalized() {
    let scratch = Scratch::new("fallback");
    let d = &scratch.0;
    let mnt = d.join("media/USB");
    std::fs::create_dir_all(&mnt).expect("mount");
    std::fs::create_dir_all(d.join("home/src")).expect("home");
    let target = fallback_target(&d.join("home/src/.."), &[mnt], None, Path::exists);
    assert_eq!(target, real_location(&d.join("home")));
    assert!(
        target
            .components()
            .all(|part| part != std::path::Component::ParentDir)
    );
}

const DUAL_BOOT: &str = r#"{"blockdevices":[
  {"name":"nvme0n1","path":"/dev/nvme0n1","type":"disk","rm":false,"hotplug":false,"size":1000,"mountpoints":[null],
   "children":[
    {"name":"nvme0n1p1","path":"/dev/nvme0n1p1","type":"part","fstype":"vfat","size":1,"mountpoints":["/boot"]},
    {"name":"nvme0n1p2","path":"/dev/nvme0n1p2","type":"part","fstype":"btrfs","size":1,"mountpoints":["/","/home","/nix/store"]},
    {"name":"nvme0n1p3","path":"/dev/nvme0n1p3","type":"part","fstype":"ntfs","label":"Windows","size":1,"mountpoints":[null]},
    {"name":"nvme0n1p4","path":"/dev/nvme0n1p4","type":"part","fstype":"swap","size":1,"mountpoints":["[SWAP]"]}]},
  {"name":"sdb","path":"/dev/sdb","type":"disk","rm":"1","hotplug":"1","size":"15000","mountpoints":[null],
   "children":[{"name":"sdb1","path":"/dev/sdb1","type":"part","fstype":"vfat","label":"K","size":"15000","mountpoint":"/run/media/u/K"}]}]}"#;

#[test]
fn a_dual_boot_system_disk_lists_the_others_and_is_never_ejectable() {
    let nodes = parse_lsblk(DUAL_BOOT).expect("valid lsblk");
    let listed: Vec<_> = devices(&nodes)
        .iter()
        .map(|device| (device.path.display().to_string(), device.ejectable))
        .collect();
    assert_eq!(
        listed,
        [
            ("/dev/nvme0n1p3".to_owned(), false),
            ("/dev/sdb1".to_owned(), true)
        ]
    );
    assert!(eject_plan(&nodes, Path::new("/dev/nvme0n1"), "x").is_none());
    let plan = eject_plan(&nodes, Path::new("/dev/sdb"), "K").expect("sdb is ejectable");
    assert_eq!(plan.mounts(), [PathBuf::from("/run/media/u/K")]);
}

#[test]
fn malformed_lsblk_output_is_an_error_not_a_panic() {
    assert!(parse_lsblk("{").is_err());
    assert!(parse_lsblk(r#"{"blockdevices":{}}"#).is_err());
    assert!(parse_lsblk(r#"{"blockdevices":[{"name":5}]}"#).is_err());
}
