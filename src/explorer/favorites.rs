//! The favorites at run time: the sidebar list mirrors the GTK bookmarks
//! file ([`fileflux::core::bookmarks`]). Adding or removing a favorite shows
//! at once and writes the file off the UI thread; an edit of the file by
//! another program (Nautilus, a text editor) shows live, through the same
//! watcher thread the folders use.
//!
//! The file lives in `gtk-3.0/`, which may not exist yet: its parent is
//! watched until it does.

use std::path::{Path, PathBuf};

use fileflux::core::Location;
use fileflux::core::bookmarks::{self, Bookmark};
use fileflux::core::watch::{WatchHandle, WatchHub};
use gpui::Context;

use super::state::Favorites;
use super::view::Explorer;

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
        let Some(file) = self.bookmarks.path.clone() else {
            return;
        };
        let folder = folder.clone();
        let write = cx.background_executor().spawn(async move {
            if favorite {
                bookmarks::add(&file, &folder)
            } else {
                bookmarks::remove(&file, &folder)
            }
        });
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
            .spawn(async move { bookmarks::read(&file) });
        cx.spawn(async move |this, cx| {
            let read = read.await;
            if let Err(closed) = this.update(cx, |this, cx| match read {
                Ok(list) => {
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
