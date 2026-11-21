//! User and group names for the numeric ids a scan reads.
//!
//! One [`IdResolver`] serves a whole scan: it parses `/etc/passwd` and
//! `/etc/group` once, on the first lookup, and remembers every answer, so a
//! folder of thousands of entries costs one read of each file and one name
//! allocation per distinct id — never a file read per row or per frame.

use std::collections::HashMap;
use std::sync::Arc;

use super::perms::parse_id_file;

/// Resolves uids and gids to names, falling back to the number.
#[derive(Debug, Default)]
pub struct IdResolver {
    /// Read `/etc/passwd` and `/etc/group` on the first lookup.
    from_system: bool,
    loaded: bool,
    users: HashMap<u32, Arc<str>>,
    groups: HashMap<u32, Arc<str>>,
}

impl IdResolver {
    /// A resolver over the system's `/etc/passwd` and `/etc/group`, read on
    /// the first lookup (a scan of a folder with no entries reads nothing).
    #[must_use]
    pub fn system() -> Self {
        Self {
            from_system: true,
            ..Self::default()
        }
    }

    /// A resolver over the given passwd and group texts.
    #[must_use]
    pub fn from_texts(passwd: &str, group: &str) -> Self {
        let mut resolver = Self {
            loaded: true,
            ..Self::default()
        };
        resolver.users = names_of(passwd);
        resolver.groups = names_of(group);
        resolver
    }

    fn load(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        if self.from_system {
            let read = |file: &str| std::fs::read_to_string(file).unwrap_or_default();
            self.users = names_of(&read("/etc/passwd"));
            self.groups = names_of(&read("/etc/group"));
        }
    }

    /// The user name of `uid`, or the number when it has none.
    pub fn user(&mut self, uid: u32) -> Arc<str> {
        self.load();
        Arc::clone(
            self.users
                .entry(uid)
                .or_insert_with(|| Arc::from(uid.to_string())),
        )
    }

    /// The group name of `gid`, or the number when it has none.
    pub fn group(&mut self, gid: u32) -> Arc<str> {
        self.load();
        Arc::clone(
            self.groups
                .entry(gid)
                .or_insert_with(|| Arc::from(gid.to_string())),
        )
    }
}

/// The names of an id file; the first line of an id wins, like `getpwuid`.
fn names_of(text: &str) -> HashMap<u32, Arc<str>> {
    let mut names = HashMap::new();
    for (id, name) in parse_id_file(text) {
        names.entry(id).or_insert_with(|| Arc::from(name));
    }
    names
}

/// The users an owner menu offers, from the parsed `/etc/passwd`: people
/// first (uid 1000 up, by name), then root, then the system accounts
/// (by name). Each uid once, the first line winning like `getpwuid`; the
/// overflow uid 65534 counts as a system account.
#[must_use]
pub fn owner_choices(users: &[(u32, String)]) -> Vec<(u32, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut choices: Vec<(u32, String)> = users
        .iter()
        .filter(|(uid, _)| seen.insert(*uid))
        .cloned()
        .collect();
    let rank = |uid: u32| match uid {
        1000..=65533 => 0,
        0 => 1,
        _ => 2,
    };
    choices.sort_by(|(a_uid, a_name), (b_uid, b_name)| {
        (rank(*a_uid), a_name, a_uid).cmp(&(rank(*b_uid), b_name, b_uid))
    });
    choices
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "# users\nroot:x:0:0:root:/root:/bin/sh\nana:x:1000:100:Ana:/home/ana:/bin/zsh\nana2:x:1000:100::/h:/bin/sh\n";
    const GROUP: &str = "root:x:0:\nusers:x:100:ana\n";

    #[test]
    fn known_ids_resolve_to_names_and_the_first_line_of_an_id_wins() {
        let mut ids = IdResolver::from_texts(PASSWD, GROUP);
        assert_eq!(&*ids.user(0), "root");
        assert_eq!(&*ids.user(1000), "ana");
        assert_eq!(&*ids.group(100), "users");
    }

    #[test]
    fn unknown_ids_show_their_number() {
        let mut ids = IdResolver::from_texts(PASSWD, GROUP);
        assert_eq!(&*ids.user(4242), "4242");
        assert_eq!(&*ids.group(77), "77");
        // A user id and a group id are separate namespaces.
        assert_eq!(&*ids.group(1000), "1000");
    }

    #[test]
    fn answers_are_cached_and_shared() {
        let mut ids = IdResolver::from_texts(PASSWD, GROUP);
        let first = ids.user(1000);
        assert!(
            Arc::ptr_eq(&first, &ids.user(1000)),
            "one allocation per id"
        );
        let number = ids.user(9);
        assert!(Arc::ptr_eq(&number, &ids.user(9)), "numbers are cached too");
    }

    #[test]
    fn empty_texts_resolve_every_id_to_its_number() {
        let mut ids = IdResolver::from_texts("", "");
        assert_eq!(&*ids.user(0), "0");
        assert_eq!(&*ids.group(0), "0");
    }

    #[test]
    fn owner_choices_list_people_then_root_then_system_accounts_once_each() {
        let users = parse_id_file(
            "root:x:0:0::/root:/bin/sh\ndaemon:x:1:1::/:/sbin/nologin\nzoe:x:1001:100::/h:/bin/sh\nana:x:1000:100::/h:/bin/sh\nana2:x:1000:100::/h:/bin/sh\nnobody:x:65534:65534::/:/bin/false\n",
        );
        let choices = owner_choices(&users);
        let names: Vec<&str> = choices.iter().map(|(_, name)| name.as_str()).collect();
        assert_eq!(names, ["ana", "zoe", "root", "daemon", "nobody"]);
    }

    #[test]
    fn owner_choices_of_nothing_is_empty() {
        assert!(owner_choices(&[]).is_empty());
    }
}
