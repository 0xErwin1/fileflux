//! Type-ahead find: typing a name moves the selection to the first entry
//! whose name starts with what was typed (GNOME Files / PCManFM). The
//! matching is pure: the caller passes the names in display order (so sort,
//! filters and hidden files are already applied), the current position and
//! the time, and gets the position to select.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Idle time after which the next character starts a new prefix.
pub(crate) const RESET_AFTER: Duration = Duration::from_secs(1);

/// The prefix typed so far.
#[derive(Debug, Default)]
pub(crate) struct TypeAhead {
    /// Lowercased prefix.
    buffer: String,
    last_typed: Option<Instant>,
    /// The cursor path this buffer last moved the selection to (or found
    /// the selection on). A different cursor means the selection was changed
    /// by something else (mouse, arrows, navigation) and the buffer is stale.
    cursor: Option<PathBuf>,
}

impl TypeAhead {
    /// The prefix currently buffered (lowercased); empty when reset.
    pub(crate) fn prefix(&self) -> &str {
        &self.buffer
    }

    /// Forgets the typed prefix.
    pub(crate) fn reset(&mut self) {
        self.buffer.clear();
        self.last_typed = None;
        self.cursor = None;
    }

    /// Forgets the prefix when the selection's cursor is not where the last
    /// press left it.
    pub(crate) fn sync_cursor(&mut self, cursor: Option<&Path>) {
        if self.cursor.as_deref() != cursor {
            self.reset();
        }
    }

    /// Remembers where the selection's cursor is after a press.
    pub(crate) fn remember_cursor(&mut self, cursor: Option<PathBuf>) {
        self.cursor = cursor;
    }

    /// Appends `ch` and returns the position to select, or `None` to keep
    /// the current selection. `names` are the visible entries in display
    /// order; `current` is the selection's cursor position.
    ///
    /// The same character typed repeatedly cycles through the entries that
    /// start with it, after `current`; anything else selects the first entry
    /// starting with the whole prefix.
    pub(crate) fn press<N: AsRef<str>>(
        &mut self,
        ch: char,
        now: Instant,
        names: &[N],
        current: Option<usize>,
    ) -> Option<usize> {
        let expired = self
            .last_typed
            .is_none_or(|last| now.saturating_duration_since(last) > RESET_AFTER);
        if expired {
            self.buffer.clear();
        }
        self.last_typed = Some(now);
        self.buffer.extend(ch.to_lowercase());

        let mut chars = self.buffer.chars();
        let first = chars.next()?;
        let repeated = self.buffer.chars().count() > 1 && chars.all(|c| c == first);
        if repeated {
            let start = current.map_or(0, |at| at + 1);
            let single = first.to_string();
            return (0..names.len())
                .map(|offset| (start + offset) % names.len())
                .find(|&at| {
                    names
                        .get(at)
                        .is_some_and(|name| starts_with(name.as_ref(), &single))
                });
        }
        names
            .iter()
            .position(|name| starts_with(name.as_ref(), &self.buffer))
    }
}

fn starts_with(name: &str, lowercase_prefix: &str) -> bool {
    name.to_lowercase().starts_with(lowercase_prefix)
}

/// Whether `text` (a key's produced text) is one typeable character: a
/// single non-control character. A leading space is not type-ahead.
pub(crate) fn typeable(text: &str, buffer_active: bool) -> Option<char> {
    let mut chars = text.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || ch.is_control() || (ch == ' ' && !buffer_active) {
        return None;
    }
    Some(ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 6] = ["alpha", "Apple", "bravo", "Éclair", "avocado", "zeta"];

    fn at(seconds: f32) -> Instant {
        // One fixed origin so injected times are comparable.
        static ORIGIN: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
        *ORIGIN + Duration::from_secs_f32(seconds)
    }

    #[test]
    fn prefix_selects_first_match_in_display_order() {
        let mut find = TypeAhead::default();
        assert_eq!(find.press('b', at(0.0), &NAMES, Some(0)), Some(2));
        assert_eq!(find.press('r', at(0.2), &NAMES, Some(2)), Some(2));
        let mut find = TypeAhead::default();
        assert_eq!(find.press('a', at(0.0), &NAMES, None), Some(0));
        assert_eq!(find.press('v', at(0.1), &NAMES, Some(0)), Some(4));
    }

    #[test]
    fn matching_ignores_case_and_is_unicode_aware() {
        let mut find = TypeAhead::default();
        assert_eq!(find.press('A', at(0.0), &NAMES, None), Some(0));
        assert_eq!(find.press('P', at(0.1), &NAMES, Some(0)), Some(1));
        let mut find = TypeAhead::default();
        assert_eq!(find.press('é', at(0.0), &NAMES, None), Some(3));
        let mut find = TypeAhead::default();
        assert_eq!(find.press('É', at(0.0), &NAMES, None), Some(3));
    }

    #[test]
    fn no_match_keeps_the_selection() {
        let mut find = TypeAhead::default();
        assert_eq!(find.press('q', at(0.0), &NAMES, Some(2)), None);
        assert_eq!(find.press('b', at(5.0), &NAMES, Some(2)), Some(2));
        assert_eq!(find.press('x', at(5.1), &NAMES, Some(2)), None);
    }

    #[test]
    fn repeating_one_character_cycles_through_its_entries() {
        let mut find = TypeAhead::default();
        let names = ["alpha", "Apple", "bravo", "avocado"];
        assert_eq!(find.press('a', at(0.0), &names, None), Some(0));
        assert_eq!(find.press('a', at(0.1), &names, Some(0)), Some(1));
        assert_eq!(find.press('a', at(0.2), &names, Some(1)), Some(3));
        assert_eq!(find.press('a', at(0.3), &names, Some(3)), Some(0));
        let mut find = TypeAhead::default();
        assert_eq!(find.press('z', at(0.0), &names, Some(0)), None);
        assert_eq!(find.press('z', at(0.1), &names, Some(0)), None);
    }

    #[test]
    fn typing_after_the_timeout_starts_a_new_prefix() {
        let mut find = TypeAhead::default();
        assert_eq!(find.press('a', at(0.0), &NAMES, None), Some(0));
        // Within the second: "av" extends the prefix.
        assert_eq!(find.press('v', at(0.9), &NAMES, Some(0)), Some(4));
        assert_eq!(find.prefix(), "av");
        // More than a second later: "b" alone, not "avb".
        assert_eq!(find.press('b', at(2.5), &NAMES, Some(4)), Some(2));
        assert_eq!(find.prefix(), "b");
    }

    #[test]
    fn only_the_names_passed_are_matched() {
        // The caller passes the visible entries; "alpha" is hidden or
        // filtered out here, so it can never be selected.
        let visible = ["Apple", "bravo"];
        let mut find = TypeAhead::default();
        assert_eq!(find.press('a', at(0.0), &visible, None), Some(0));
        assert_eq!(find.press('l', at(0.1), &visible, Some(0)), None);
    }

    #[test]
    fn a_moved_cursor_or_reset_forgets_the_prefix() {
        let mut find = TypeAhead::default();
        find.press('a', at(0.0), &NAMES, None);
        find.remember_cursor(Some(PathBuf::from("/d/alpha")));
        find.sync_cursor(Some(Path::new("/d/alpha")));
        assert_eq!(find.prefix(), "a");
        find.sync_cursor(Some(Path::new("/d/bravo")));
        assert_eq!(find.prefix(), "");
        find.press('a', at(0.1), &NAMES, None);
        find.reset();
        assert_eq!(find.prefix(), "");
    }

    #[test]
    fn typeable_accepts_one_printable_character() {
        assert_eq!(typeable("a", false), Some('a'));
        assert_eq!(typeable("é", false), Some('é'));
        assert_eq!(typeable(" ", false), None);
        assert_eq!(typeable(" ", true), Some(' '));
        assert_eq!(typeable("\u{7f}", true), None);
        assert_eq!(typeable("ab", false), None);
        assert_eq!(typeable("", false), None);
    }
}
