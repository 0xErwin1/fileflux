//! In-memory navigation history over [`Location`]s for the Fileflux explorer.
//!
//! [`Navigation`] is a pure state machine: it never touches the filesystem and
//! never fails. The UI owns the commit pattern — clone the navigation, apply
//! the intended transition to the clone, scan the target location off the UI
//! thread, and only replace the committed state with the clone after the
//! listing succeeded. A failed scan must leave both the original and the
//! candidate untouched, so history, selection and the current location stay
//! consistent (spec S2 async navigation guard).

use std::path::Path;

use super::Location;

/// Back/forward navigation history over [`Location`]s.
///
/// Invariants:
/// - `visit` records the previous location in the back stack and truncates the
///   forward branch; visiting the current location is a no-op (no duplicates).
/// - `back`/`forward` swap between the stacks and return `false` when the
///   target stack is empty, leaving state unchanged.
/// - `up` is a regular `visit` of the parent directory, so it joins history;
///   it is a no-op at the filesystem root and in the virtual
///   [`Location::Trash`] and [`Location::Recent`]; from a
///   [`Location::Search`] it goes to the folder searched.
/// - `replace_current` changes what is shown without touching the history:
///   a search that is refined stays one history entry.
#[derive(Clone, Debug)]
pub struct Navigation {
    current: Location,
    back: Vec<Location>,
    forward: Vec<Location>,
}

impl Navigation {
    /// Starts at `current` with empty history.
    pub fn new(current: Location) -> Self {
        Self {
            current,
            back: Vec::new(),
            forward: Vec::new(),
        }
    }

    /// The committed current location.
    pub fn current(&self) -> &Location {
        &self.current
    }

    /// Navigates to `location`, pushing the previous one onto the back stack
    /// and truncating the forward branch. Visiting the location we are already
    /// in changes nothing.
    pub fn visit(&mut self, location: Location) {
        if location == self.current {
            return;
        }
        self.back.push(self.current.clone());
        self.forward.clear();
        self.current = location;
    }

    /// Replaces the current location in place: the back and forward stacks
    /// stay as they are. A search refined by typing is one history entry,
    /// not one per keystroke.
    pub fn replace_current(&mut self, location: Location) {
        self.current = location;
    }

    /// Steps one entry back; returns `false` (state unchanged) if none exist.
    pub fn back(&mut self) -> bool {
        let Some(previous) = self.back.pop() else {
            return false;
        };
        let here = std::mem::replace(&mut self.current, previous);
        self.forward.push(here);
        true
    }

    /// Steps one entry forward; returns `false` (state unchanged) if none exist.
    pub fn forward(&mut self) -> bool {
        let Some(next) = self.forward.pop() else {
            return false;
        };
        let here = std::mem::replace(&mut self.current, next);
        self.back.push(here);
        true
    }

    /// Visits the parent of a directory location.
    ///
    /// Returns `false` without changing state in [`Location::Trash`] or
    /// [`Location::Recent`], at the
    /// filesystem root, or when the parent resolves to the current location.
    /// A [`Location::Search`] or [`Location::Usage`] goes up to the folder it
    /// searches or measures.
    pub fn up(&mut self) -> bool {
        let parent = match &self.current {
            Location::Directory(path) => path.parent().map(Path::to_path_buf),
            Location::Search { root, .. } | Location::Usage { root } => Some(root.clone()),
            Location::Trash | Location::Recent => None,
        };
        let Some(parent) = parent else {
            return false;
        };
        if Location::Directory(parent.clone()) == self.current {
            return false;
        }
        self.visit(Location::Directory(parent));
        true
    }

    /// Whether `back` can step.
    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    /// Whether `forward` can step.
    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Whether `up` would change the location (never true in Trash or at root).
    pub fn can_up(&self) -> bool {
        match &self.current {
            Location::Directory(path) => path
                .parent()
                .is_some_and(|parent| Location::Directory(parent.to_path_buf()) != self.current),
            Location::Search { .. } | Location::Usage { .. } => true,
            Location::Trash | Location::Recent => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::Location;
    use super::Navigation;

    fn dir(path: &str) -> Location {
        Location::Directory(PathBuf::from(path))
    }

    #[test]
    fn navigation_is_cloneable_debug_and_snapshot_independent() {
        let mut nav = Navigation::new(Location::Trash);
        assert_eq!(nav.current(), &Location::Trash);
        assert!(!nav.can_back());
        assert!(!nav.can_forward());

        let snapshot = nav.clone();
        nav.visit(dir("/tmp/a"));

        assert_eq!(snapshot.current(), &Location::Trash);
        assert!(
            !snapshot.can_back(),
            "clone snapshot must not follow mutations"
        );
        assert!(nav.can_back());
        assert!(format!("{nav:?}").contains("Navigation"), "Debug available");
    }

    #[test]
    fn visit_truncates_forward_branch_and_ignores_same_location() {
        let home = dir("/home");
        let docs = dir("/home/docs");
        let pics = dir("/home/pics");

        let mut nav = Navigation::new(home.clone());
        nav.visit(docs.clone());
        nav.visit(pics.clone());
        assert!(nav.back());
        assert_eq!(nav.current(), &docs);
        assert!(
            nav.can_forward(),
            "forward branch exists before a new visit"
        );

        nav.visit(home.clone());
        assert_eq!(nav.current(), &home);
        assert!(!nav.can_forward(), "visit must truncate the forward branch");
        // Prior history survives the truncation: two back steps remain.
        assert!(nav.back());
        assert_eq!(nav.current(), &docs);
        assert!(nav.back());
        assert_eq!(nav.current(), &home);
        assert!(!nav.can_back());

        // Same-location visit must not duplicate history.
        let mut nav = Navigation::new(home.clone());
        nav.visit(docs.clone());
        assert!(nav.can_back());
        nav.visit(docs.clone());
        assert_eq!(nav.current(), &docs);
        assert!(
            nav.can_back(),
            "same-location visit must not push a duplicate"
        );
        assert!(nav.back());
        assert_eq!(nav.current(), &home);
        assert!(!nav.can_back());
    }

    #[test]
    fn back_and_forward_walk_history_without_losing_it() {
        let home = dir("/home");
        let docs = dir("/home/docs");

        let mut nav = Navigation::new(home.clone());
        nav.visit(docs.clone());
        assert!(nav.back());
        assert_eq!(nav.current(), &home);
        assert!(!nav.can_back());
        assert!(nav.forward());
        assert_eq!(nav.current(), &docs);
        assert!(!nav.can_forward());
        assert!(nav.can_back());
        // Repeated drains stay false without corrupting state.
        assert!(!nav.forward());
        assert_eq!(nav.current(), &docs);
    }

    #[test]
    fn up_navigates_to_parent_and_records_history() {
        let leaf = dir("/srv/data/deep");
        let mut nav = Navigation::new(leaf.clone());

        assert!(nav.up());
        assert_eq!(nav.current(), &dir("/srv/data"));
        assert!(nav.can_back());
        assert!(nav.back());
        assert_eq!(nav.current(), &leaf);
    }

    #[test]
    fn up_is_noop_at_root_and_in_trash() {
        let mut root_nav = Navigation::new(dir("/"));
        assert!(!root_nav.up(), "root Up is a no-op");
        assert_eq!(root_nav.current(), &dir("/"));
        assert!(!root_nav.can_back(), "no-op Up must not grow history");

        let mut trash_nav = Navigation::new(Location::Trash);
        assert!(!trash_nav.up(), "Trash Up is a no-op");
        assert_eq!(trash_nav.current(), &Location::Trash);
        assert!(!trash_nav.can_back());

        let mut recent_nav = Navigation::new(Location::Recent);
        assert!(!recent_nav.can_up());
        assert!(!recent_nav.up(), "Recent Up is a no-op");
        assert_eq!(recent_nav.current(), &Location::Recent);
        assert!(!recent_nav.can_back());
    }

    fn search(root: &str, query: &str) -> Location {
        Location::Search {
            root: PathBuf::from(root),
            query: query.to_owned(),
        }
    }

    #[test]
    fn a_search_goes_up_to_its_folder_and_back_returns_to_it() {
        let mut nav = Navigation::new(dir("/srv/data"));
        nav.visit(search("/srv/data", "re"));
        assert!(nav.can_up());
        assert!(nav.up());
        assert_eq!(nav.current(), &dir("/srv/data"));
        assert!(nav.back());
        assert_eq!(nav.current(), &search("/srv/data", "re"));
        assert!(nav.back());
        assert_eq!(nav.current(), &dir("/srv/data"));
    }

    #[test]
    fn replacing_the_current_location_keeps_the_history() {
        let mut nav = Navigation::new(dir("/a"));
        nav.visit(search("/a", "r"));
        nav.replace_current(search("/a", "re"));
        nav.replace_current(search("/a", "rep"));
        assert_eq!(nav.current(), &search("/a", "rep"));
        assert!(nav.back(), "one step back, not one per keystroke");
        assert_eq!(nav.current(), &dir("/a"));
        assert!(!nav.can_back());
        assert!(nav.forward());
        assert_eq!(nav.current(), &search("/a", "rep"));
    }

    #[test]
    fn only_recent_and_search_list_files_from_elsewhere() {
        assert!(Location::Recent.lists_files_from_elsewhere());
        assert!(search("/a", "x").lists_files_from_elsewhere());
        assert!(!Location::Trash.lists_files_from_elsewhere());
        assert!(!dir("/a").lists_files_from_elsewhere());
    }
}
