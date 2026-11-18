//! Which images the viewer walks through, and how the arrows move.

use std::cmp::Ordering;
use std::io;
use std::path::{Path, PathBuf};

use fileflux::cli::{ImageList, ViewerLaunch};
use fileflux::core::images::is_image_name;

/// A navigation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Previous,
    Next,
    First,
    Last,
}

/// The explorer's name order: case-insensitive by name, raw path to break ties.
pub(crate) fn compare_names(a: &Path, b: &Path) -> Ordering {
    let lowered = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    lowered(a).cmp(&lowered(b)).then_with(|| a.cmp(b))
}

pub(crate) fn sort_images(paths: &mut [PathBuf]) {
    paths.sort_by(|a, b| compare_names(a, b));
}

/// The images in `dir`, in the explorer's name order. Entries that cannot
/// be inspected are skipped; a directory that cannot be read is an error.
pub(crate) fn sibling_images(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut images = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        if !is_image_name(&entry.file_name()) {
            continue;
        }
        // `metadata` follows symlinks, so a link to an image counts and a
        // folder named like an image does not.
        if std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_file()) {
            images.push(entry.path());
        }
    }
    sort_images(&mut images);
    Ok(images)
}

/// The navigation list and the starting index for a launch: the given
/// images as they are, or the images next to the first one (`siblings`
/// reads the folder). A folder that cannot be read, or a file that is not
/// in its listing, still leaves the first image shown and navigable.
pub(crate) fn plan(
    launch: &ViewerLaunch,
    siblings: impl FnOnce(&Path) -> io::Result<Vec<PathBuf>>,
) -> (Vec<PathBuf>, usize) {
    let Some(first) = launch.images.first() else {
        return (Vec::new(), 0);
    };
    if launch.list == ImageList::Given {
        return (launch.images.clone(), 0);
    }
    let dir = first.parent().unwrap_or_else(|| Path::new("/"));
    let mut list = siblings(dir).unwrap_or_else(|error| {
        log::warn!("cannot list {}: {error}", dir.display());
        Vec::new()
    });
    if locate(&list, first).is_none() {
        list.push(first.clone());
        sort_images(&mut list);
    }
    let index = locate(&list, first).unwrap_or(0);
    (list, index)
}

/// The position of `current` in `list`.
pub(crate) fn locate(list: &[PathBuf], current: &Path) -> Option<usize> {
    list.iter().position(|path| path == current)
}

/// Where `step` leads from `index` in a list of `len`, or `None` when it
/// would not move: navigation stops at both ends instead of wrapping.
pub(crate) fn target(index: usize, len: usize, step: Step) -> Option<usize> {
    let last = len.checked_sub(1)?;
    let wanted = match step {
        Step::Previous => index.checked_sub(1)?,
        Step::Next => index.saturating_add(1).min(last),
        Step::First => 0,
        Step::Last => last,
    };
    (wanted != index).then_some(wanted)
}

/// The indices worth preloading around `index`.
pub(crate) fn neighbors(index: usize, len: usize) -> Vec<usize> {
    [
        target(index, len, Step::Next),
        target(index, len, Step::Previous),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// `3 / 42` for the zero-based `index`.
pub(crate) fn position_label(index: usize, len: usize) -> String {
    format!("{} / {len}", index + 1)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|name| PathBuf::from(format!("/p/{name}")))
            .collect()
    }

    #[test]
    fn names_sort_case_insensitively_like_the_explorer() {
        let mut list = paths(&["b.png", "A.png", "a10.png", "C.PNG", "a2.png"]);
        sort_images(&mut list);
        assert_eq!(
            list,
            paths(&["A.png", "a10.png", "a2.png", "b.png", "C.PNG"])
        );
    }

    #[test]
    fn equal_names_in_different_case_fall_back_to_the_raw_path() {
        let mut list = paths(&["x.png", "X.png"]);
        sort_images(&mut list);
        assert_eq!(list, paths(&["X.png", "x.png"]));
    }

    #[test]
    fn navigation_stops_at_the_ends() {
        assert_eq!(target(0, 5, Step::Previous), None);
        assert_eq!(target(4, 5, Step::Next), None);
        assert_eq!(target(2, 5, Step::Previous), Some(1));
        assert_eq!(target(2, 5, Step::Next), Some(3));
        assert_eq!(target(2, 5, Step::First), Some(0));
        assert_eq!(target(2, 5, Step::Last), Some(4));
        assert_eq!(target(0, 5, Step::First), None);
        assert_eq!(target(4, 5, Step::Last), None);
        assert_eq!(target(0, 0, Step::Next), None);
        assert_eq!(target(0, 1, Step::Last), None);
    }

    #[test]
    fn preload_targets_are_the_existing_neighbors() {
        assert_eq!(neighbors(0, 1), Vec::<usize>::new());
        assert_eq!(neighbors(0, 3), vec![1]);
        assert_eq!(neighbors(1, 3), vec![2, 0]);
        assert_eq!(neighbors(2, 3), vec![1]);
    }

    #[test]
    fn position_is_one_based() {
        assert_eq!(position_label(2, 42), "3 / 42");
        assert_eq!(position_label(0, 1), "1 / 1");
    }

    fn launch(images: &[&str], list: ImageList) -> ViewerLaunch {
        ViewerLaunch {
            images: paths(images),
            list,
        }
    }

    #[test]
    fn a_given_set_is_the_list_in_the_given_order() {
        let read = std::cell::Cell::new(false);
        let (list, index) = plan(&launch(&["z.png", "a.png"], ImageList::Given), |_| {
            read.set(true);
            Ok(Vec::new())
        });
        assert_eq!(list, paths(&["z.png", "a.png"]));
        assert_eq!(index, 0);
        assert!(!read.get(), "the folder is not read");
    }

    #[test]
    fn a_single_image_browses_its_folder_starting_at_itself() {
        let (list, index) = plan(&launch(&["b.png"], ImageList::Siblings), |dir| {
            assert_eq!(dir, Path::new("/p"));
            Ok(paths(&["a.png", "b.png", "c.png"]))
        });
        assert_eq!(list, paths(&["a.png", "b.png", "c.png"]));
        assert_eq!(index, 1);
    }

    #[test]
    fn an_unlisted_or_unreadable_start_is_still_shown() {
        let missing = plan(&launch(&["b.png"], ImageList::Siblings), |_| {
            Ok(paths(&["a.png", "c.png"]))
        });
        assert_eq!(missing, (paths(&["a.png", "b.png", "c.png"]), 1));
        let failing = plan(&launch(&["b.png"], ImageList::Siblings), |_| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert_eq!(failing, (paths(&["b.png"]), 0));
    }

    #[test]
    fn locate_finds_the_current_path() {
        let list = paths(&["a.png", "b.png"]);
        assert_eq!(locate(&list, Path::new("/p/b.png")), Some(1));
        assert_eq!(locate(&list, Path::new("/p/c.png")), None);
    }

    #[test]
    fn siblings_are_the_sorted_image_files_of_the_folder() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = std::env::temp_dir().join(format!("fileflux-viewer-nav-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(dir.join("folder.png"))?;
        for name in ["b.png", "A.JPG", "notes.txt", "c.gif"] {
            std::fs::write(dir.join(name), b"x")?;
        }
        let found = sibling_images(&dir)?;
        std::fs::remove_dir_all(&dir)?;
        let names: Vec<_> = found
            .iter()
            .filter_map(|path| path.file_name()?.to_str())
            .collect();
        assert_eq!(names, ["A.JPG", "b.png", "c.gif"]);
        Ok(())
    }
}
