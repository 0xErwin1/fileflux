//! The favorites at run time: the sidebar list mirrors the GTK bookmarks
//! file ([`fileflux::core::bookmarks`]). Adding or removing a favorite shows
//! at once and writes the file off the UI thread; an edit of the file by
//! another program (Nautilus, a text editor) shows live, through the same
//! watcher thread the folders use.
//!
//! The file lives in `gtk-3.0/`, which may not exist yet: its parent is
//! watched until it does.

use std::io;
use std::path::{Path, PathBuf};

use bolt::controls::{InputEvent, InputState};
use fileflux::core::Location;
use fileflux::core::bookmarks::{self, Bookmark};
use fileflux::core::watch::{WatchHandle, WatchHub};
use gpui::{AppContext as _, Bounds, Context, Entity, Pixels, Point, Window};

use super::ops::{DialogAction, DialogKind, DialogState};
use super::state::{FavoriteMove, Favorites};
use super::view::Explorer;

/// The element id of the Favorites section's row number `index`.
pub(crate) fn favorite_row_id(index: usize) -> String {
    format!("{FAVORITE_ROW_PREFIX}{index}")
}

const FAVORITE_ROW_PREFIX: &str = "favorite-";

/// The position in the Favorites section of the row with element id `row`;
/// `None` for any other sidebar row.
pub(crate) fn favorite_row_index(row: &str) -> Option<usize> {
    row.strip_prefix(FAVORITE_ROW_PREFIX)?.parse().ok()
}

/// Whether a drag of a folder reorders the favorites: only one that began on
/// a favorite row of the sidebar, and only of a folder that is a favorite. A
/// folder dragged from the entries area (or from another sidebar row) keeps
/// the ordinary drop everywhere, the favorite rows' edges included.
pub(crate) fn drag_reorders(began_on_favorite_row: bool, folder_is_favorite: bool) -> bool {
    began_on_favorite_row && folder_is_favorite
}

/// The open "Rename…" dialog of a favorite: which folder and its one field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenameDialog {
    pub(crate) folder: PathBuf,
    pub(crate) input: Entity<InputState>,
}

/// The pointer is over a favorite row during a drag of a favorite: the row's
/// place and bounds, and the gap between rows it points at (see
/// [`insertion_gap`]) when dropping there would reorder the list.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FavoriteInsert {
    pub(crate) folder: PathBuf,
    pub(crate) row: usize,
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) gap: Option<usize>,
}

/// How far from a row's top or bottom edge a pointer still means "between
/// rows" for a favorite drag, in pixels; the rest of the row is a drop on it.
pub(crate) const INSERT_BAND: f32 = 7.0;

/// The gap of the Favorites list a pointer at `pointer` points at while over
/// the favorite row number `row` of `bounds`: the row's top band is the gap
/// above it, its bottom band the gap below it, the middle none (a drop
/// there is a drop on the row).
pub(crate) fn insertion_gap(
    row: usize,
    bounds: Bounds<Pixels>,
    pointer: Point<Pixels>,
) -> Option<usize> {
    if !bounds.contains(&pointer) {
        return None;
    }
    let band = INSERT_BAND.min(bounds.size.height.as_f32() / 4.0);
    let from_top = (pointer.y - bounds.origin.y).as_f32();
    if from_top < band {
        Some(row)
    } else if from_top > bounds.size.height.as_f32() - band {
        Some(row + 1)
    } else {
        None
    }
}

/// Where the bookmarks file is and what watches it.
#[derive(Debug)]
pub(crate) struct BookmarkSync {
    /// The bookmarks file; `None` without `$HOME` (the favorites then live
    /// only for this run).
    path: Option<PathBuf>,
    /// The hold on the file's directory.
    dir_hold: Option<WatchHandle>,
    /// The hold on that directory's parent while the directory is missing.
    parent_hold: Option<WatchHandle>,
}

impl BookmarkSync {
    /// The bookmarks file, when there is one.
    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            dir_hold: None,
            parent_hold: None,
        }
    }

    /// The directories whose events can mean the file changed.
    fn dirs(&self) -> [Option<&Path>; 2] {
        let dir = self.path.as_deref().and_then(Path::parent);
        [
            dir.filter(|_| self.dir_hold.is_some()),
            dir.and_then(Path::parent)
                .filter(|_| self.parent_hold.is_some()),
        ]
    }

    /// Whether an event of the watcher on `folder` can be about the file.
    pub(crate) fn concerns(&self, folder: &Path) -> bool {
        self.dirs().contains(&Some(folder))
    }

    /// Watches the file's directory, or its parent while it is missing.
    /// Called again whenever either watch is lost or the directory may have
    /// appeared.
    pub(crate) fn attach(&mut self, hub: &WatchHub) {
        let Some(dir) = self.path.as_deref().and_then(Path::parent) else {
            return;
        };
        if self.dir_hold.as_ref().is_some_and(WatchHandle::is_live) {
            return;
        }
        self.dir_hold = hub.watch(dir).ok();
        self.parent_hold = if self.dir_hold.is_some() {
            None
        } else {
            dir.parent().and_then(|parent| hub.watch(parent).ok())
        };
    }
}

/// The list the sidebar shows: the bookmarks, then the favorites that could
/// not move out of the settings yet.
pub(crate) fn favorites_of(bookmarks: Vec<Bookmark>, legacy: &[PathBuf]) -> Favorites {
    let mut list = bookmarks;
    for path in legacy {
        if !list.iter().any(|known| known.path == *path) {
            list.push(Bookmark {
                path: path.clone(),
                label: None,
            });
        }
    }
    Favorites::from_bookmarks(list)
}

impl Explorer {
    /// Toggles the favorite state of `location` (a folder; never the Trash).
    pub(crate) fn toggle_favorite_of(&mut self, location: &Location, cx: &mut Context<Self>) {
        let favorite = !self.favorites.contains(location);
        self.set_favorite(location, favorite, cx);
    }

    /// Makes `location` a favorite or not: the sidebar changes now, the
    /// bookmarks file follows off the UI thread, and a failed write puts the
    /// list back as the file has it.
    pub(crate) fn set_favorite(
        &mut self,
        location: &Location,
        favorite: bool,
        cx: &mut Context<Self>,
    ) {
        let Location::Directory(folder) = location else {
            return;
        };
        if self.favorites.contains(location) == favorite {
            return;
        }
        self.favorites.set(location, favorite);
        if !favorite {
            self.legacy_favorites.retain(|path| path != folder);
        }
        cx.notify();
        let folder = folder.clone();
        self.write_bookmarks(cx, move |file| {
            if favorite {
                bookmarks::add(file, &folder)
            } else {
                bookmarks::remove(file, &folder)
            }
        });
    }

    /// Runs `write` on the bookmarks file off the UI thread, then reads the
    /// file again so the sidebar shows what it holds; a failed write says so
    /// in the status line and the list goes back to the file's.
    fn write_bookmarks(
        &mut self,
        cx: &mut Context<Self>,
        write: impl FnOnce(&Path) -> io::Result<bool> + Send + 'static,
    ) {
        let Some(file) = self.bookmarks.path.clone() else {
            return;
        };
        let write = cx.background_executor().spawn(async move { write(&file) });
        cx.spawn(async move |this, cx| {
            let written = write.await;
            if let Err(closed) = this.update(cx, |this, cx| {
                if let Err(error) = written {
                    log::warn!("the GTK bookmarks were not written: {error}");
                    this.tab_mut()
                        .state
                        .set_notice(format!("Could not save the favorite ({error})"));
                }
                // The directory may have been created by this write.
                this.attach_bookmark_watch();
                this.reload_favorites(cx);
            }) {
                log::debug!("the explorer closed before the favorite was saved: {closed}");
            }
        })
        .detach();
    }

    /// Moves the favorite `folder` (Move up, Move down, or a drop between
    /// rows): the sidebar changes now, the file follows.
    pub(crate) fn move_favorite(
        &mut self,
        folder: &Path,
        step: FavoriteMove,
        cx: &mut Context<Self>,
    ) {
        let Some(before) = self.favorites.move_anchor(folder, step) else {
            return;
        };
        self.favorites.move_before(folder, before.as_deref());
        cx.notify();
        let folder = folder.to_path_buf();
        self.write_bookmarks(cx, move |file| {
            bookmarks::reorder(file, &folder, before.as_deref())
        });
    }

    /// "Rename…" of a favorite: asks for the name the sidebar shows, with the
    /// current one selected.
    pub(crate) fn rename_favorite(
        &mut self,
        folder: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self
            .favorites
            .bookmarks()
            .iter()
            .find(|favorite| favorite.path == folder)
            .map(Bookmark::display_name)
        else {
            return;
        };
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx);
            input.set_placeholder("Name in the sidebar", window, cx);
            input.set_value(current, window, cx);
            input
        });
        cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                // Only the live dialog's own field may confirm.
                let live = this.dialogs.current().is_some_and(|dialog| {
                    matches!(&dialog.action, DialogAction::RenameFavorite(state) if state.input == *input)
                });
                if live && matches!(event, InputEvent::PressEnter { .. }) {
                    this.confirm_dialog(window, cx);
                }
            },
        )
        .detach();
        let field = input.clone();
        self.offer_dialog(
            DialogState::new(
                DialogKind::RenameFavorite,
                &[],
                DialogAction::RenameFavorite(RenameDialog { folder, input }),
            ),
            window,
            cx,
        );
        cx.defer_in(window, move |_this, window, cx| {
            field.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
        });
    }

    /// Confirming the rename dialog: closes it and writes the label. An empty
    /// text, or the folder's own name, clears the label.
    pub(crate) fn confirm_rename_favorite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(DialogAction::RenameFavorite(dialog)) =
            self.dialogs.current().map(|dialog| dialog.action.clone())
        else {
            return;
        };
        let typed = dialog.input.read(cx).value().trim().to_owned();
        self.dialogs.close();
        self.dialog_closed(window, cx);
        let folder = dialog.folder;
        let own_name = Bookmark {
            path: folder.clone(),
            label: None,
        }
        .display_name();
        let label = if typed == own_name {
            String::new()
        } else {
            typed
        };
        self.favorites
            .relabel(&folder, (!label.is_empty()).then(|| label.clone()));
        cx.notify();
        self.write_bookmarks(cx, move |file| bookmarks::rename(file, &folder, &label));
    }

    /// The gap a favorite dropped now would go into: the pointer must be in
    /// the band of the row the drag is over, and the move must change the
    /// list.
    pub(crate) fn favorite_drop_gap(&self, folder: &Path, pointer: Point<Pixels>) -> Option<usize> {
        if !self.reorders_favorites_with(folder) {
            return None;
        }
        let over = self
            .favorite_insert
            .as_ref()
            .filter(|over| over.folder == folder)?;
        let gap = insertion_gap(over.row, over.bounds, pointer)?;
        self.favorites
            .move_anchor(folder, FavoriteMove::Gap(gap))
            .map(|_| gap)
    }

    fn reorders_favorites_with(&self, folder: &Path) -> bool {
        drag_reorders(
            self.favorite_drag,
            self.favorites
                .contains(&Location::Directory(folder.to_path_buf())),
        )
    }

    /// A drag of the folder `folder` moved over the favorite row `row` (its
    /// `bounds`): remembers where it points, for the insertion line and the
    /// drop. Leaving the row forgets it.
    pub(crate) fn favorite_drag_over(
        &mut self,
        (row, bounds): (usize, Bounds<Pixels>),
        folder: &Path,
        pointer: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let next = if bounds.contains(&pointer) && self.reorders_favorites_with(folder) {
            let mut insert = FavoriteInsert {
                folder: folder.to_path_buf(),
                row,
                bounds,
                gap: None,
            };
            insert.gap = insertion_gap(row, bounds, pointer).filter(|&gap| {
                self.favorites
                    .move_anchor(folder, FavoriteMove::Gap(gap))
                    .is_some()
            });
            Some(insert)
        } else if self
            .favorite_insert
            .as_ref()
            .is_some_and(|over| over.row == row)
        {
            None
        } else {
            return;
        };
        if self.favorite_insert != next {
            self.favorite_insert = next;
            cx.notify();
        }
    }

    /// A drop of `folder` where the pointer is: reorders when it is between
    /// favorite rows; whether it did (otherwise the drop is an ordinary one).
    pub(crate) fn drop_favorite(
        &mut self,
        folder: &Path,
        pointer: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let gap = self.favorite_drop_gap(folder, pointer);
        self.favorite_insert = None;
        let Some(gap) = gap else {
            return false;
        };
        self.folder_drag = None;
        self.move_favorite(folder, FavoriteMove::Gap(gap), cx);
        true
    }

    /// Starts watching the bookmarks file's directory (see [`BookmarkSync`]).
    pub(crate) fn attach_bookmark_watch(&mut self) {
        if let Some(hub) = &self.hub {
            self.bookmarks.attach(hub);
        }
    }

    /// The watcher reported `folder`: when it concerns the bookmarks file,
    /// reads it again.
    pub(crate) fn bookmarks_folder_changed(&mut self, folder: &Path, cx: &mut Context<Self>) {
        if self.bookmarks.concerns(folder) {
            self.attach_bookmark_watch();
            self.reload_favorites(cx);
        }
    }

    /// The watcher released `folder`'s watch (it was removed or replaced).
    pub(crate) fn bookmarks_folder_gone(&mut self, folder: &Path, cx: &mut Context<Self>) {
        if self.bookmarks.concerns(folder) {
            self.bookmarks.dir_hold = None;
            self.bookmarks.parent_hold = None;
            self.attach_bookmark_watch();
            self.reload_favorites(cx);
        }
    }

    /// Reads the bookmarks file off the UI thread and shows what it holds.
    pub(crate) fn reload_favorites(&mut self, cx: &mut Context<Self>) {
        let Some(file) = self.bookmarks.path.clone() else {
            return;
        };
        let read = cx
            .background_executor()
            .spawn(async move { bookmarks::read_all(&file) });
        cx.spawn(async move |this, cx| {
            let read = read.await;
            if let Err(closed) = this.update(cx, |this, cx| match read {
                Ok((list, network)) => {
                    if network != this.network.bookmarks {
                        this.network.bookmarks = network;
                        cx.notify();
                    }
                    let favorites = favorites_of(list, &this.legacy_favorites);
                    if favorites.bookmarks() != this.favorites.bookmarks() {
                        this.favorites = favorites;
                        cx.notify();
                    }
                }
                Err(error) => log::warn!("the GTK bookmarks cannot be read: {error}"),
            }) {
                log::debug!("the explorer closed before the bookmarks were read: {closed}");
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    /// A 26 px row at y = 100.
    fn row() -> Bounds<Pixels> {
        Bounds::new(point(px(0.0), px(100.0)), size(px(200.0), px(26.0)))
    }

    fn gap_at(y: f32) -> Option<usize> {
        insertion_gap(3, row(), point(px(50.0), px(y)))
    }

    #[test]
    fn the_edges_of_a_row_point_at_the_gaps_around_it_and_the_middle_at_none() {
        assert_eq!(gap_at(100.0), Some(3), "the top edge");
        assert_eq!(gap_at(106.0), Some(3), "inside the top band");
        assert_eq!(gap_at(108.0), None, "the middle is a drop on the row");
        assert_eq!(gap_at(113.0), None);
        assert_eq!(gap_at(118.0), None);
        assert_eq!(gap_at(120.0), Some(4), "inside the bottom band");
        assert_eq!(gap_at(125.5), Some(4), "the bottom edge");
    }

    #[test]
    fn a_pointer_outside_the_row_points_at_no_gap() {
        assert_eq!(gap_at(99.0), None);
        assert_eq!(gap_at(126.5), None);
        assert_eq!(insertion_gap(3, row(), point(px(250.0), px(101.0))), None);
    }

    #[test]
    fn a_very_short_row_keeps_a_middle_to_drop_on() {
        let short = Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(8.0)));
        let at = |y: f32| insertion_gap(0, short, point(px(10.0), px(y)));
        assert_eq!(at(1.0), Some(0));
        assert_eq!(at(4.0), None);
        assert_eq!(at(7.0), Some(1));
    }

    #[test]
    fn only_a_drag_that_began_on_a_favorite_row_reorders() {
        assert!(drag_reorders(true, true));
        assert!(!drag_reorders(false, true), "from the entries area");
        assert!(!drag_reorders(true, false), "not a favorite");
        assert!(!drag_reorders(false, false));
    }

    #[test]
    fn favorite_row_ids_round_trip_and_other_rows_are_not_favorites() {
        assert_eq!(favorite_row_id(7), "favorite-7");
        assert_eq!(favorite_row_index("favorite-7"), Some(7));
        for other in [
            "place-1",
            "network-3",
            "favorite-",
            "favorite-x",
            "volume-0",
        ] {
            assert_eq!(favorite_row_index(other), None, "{other}");
        }
    }
}
