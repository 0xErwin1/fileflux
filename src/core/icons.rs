//! The icon a file's type has in the installed freedesktop icon theme: the
//! same icons pcmanfm, Nautilus and Thunar draw — Blender's for `.blend`,
//! the script glyph for a shell script, the document glyph for a PDF — so
//! a file looks the way the desktop draws it instead of only getting the
//! extension-derived bundled glyph. The theme has the last word: when it
//! names nothing for the type, [`for_file`] answers `None` and the caller
//! keeps its bundled glyph.
//!
//! Resolution is deliberately a fixed probe, not the full icon-theme spec:
//! the themes named by GTK 3's settings and their `Inherits=` chains
//! first, then `Adwaita`, `breeze`, `Papirus` and `hicolor`; and under a
//! theme, the fixed shape `<size>/<context>/<name>.<ext>`. This replaces
//! parsing every installed theme's `index.theme` to compute the whole
//! fallback forest, on purpose: the probed themes are the ones
//! distributions install with MIME icons, the fixed shape is how they lay
//! those icons out, and the answer is memoized per MIME type, so even a
//! folder of thousands of files costs one probe set per file type, never
//! per tile or per frame.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

use super::mimes;
use super::thumbnailers::data_directories;

/// The icon file for the type of a file named `name`, from the installed
/// freedesktop icon theme, or `None` when the theme names nothing for it
/// (the caller keeps its bundled glyph). Decided from the name's extension
/// alone; the filesystem is probed only for a type's first question, once
/// per process.
#[must_use]
pub fn for_file(name: &OsStr) -> Option<PathBuf> {
    let Some(mime) = mimes::of_name(name) else {
        return None;
    };
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(found) = cache.get(mime) {
        return found.clone();
    }
    let environment = environment();
    let found = resolve_in(&environment.roots, &environment.themes, mime);
    cache.insert(mime.to_owned(), found.clone());
    found
}

/// The memoized answers, keyed by MIME type (which fixes the candidate
/// names): a `None` answer is kept too, so a type the theme does not cover
/// is probed once, not per file.
static CACHE: LazyLock<Mutex<HashMap<String, Option<PathBuf>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The resolved lookup world: the theme roots and the themes to try, in
/// order. Read from the process environment once, not per call.
struct Environment {
    roots: Vec<PathBuf>,
    themes: Vec<String>,
}

/// The environment, resolved once per process.
fn environment() -> &'static Environment {
    static ENVIRONMENT: LazyLock<Environment> = LazyLock::new(|| {
        let roots: Vec<PathBuf> = data_directories()
            .into_iter()
            .map(|directory| directory.join("icons"))
            .collect();
        let themes = theme_order(&roots, configured_theme().as_deref());
        Environment { roots, themes }
    });
    &ENVIRONMENT
}

/// [`for_file`] against explicit `roots` and `themes`: the tests' way in,
/// which never touches the process environment or the memo.
#[cfg(test)]
fn for_file_in(roots: &[PathBuf], themes: &[String], name: &OsStr) -> Option<PathBuf> {
    mimes::of_name(name).and_then(|mime| resolve_in(roots, themes, mime))
}

/// `mime`'s icon file: the candidate names in turn, and under each the
/// themes in turn and the roots in turn, then the fixed sizes, contexts
/// and extensions. The first file that exists wins.
fn resolve_in(roots: &[PathBuf], themes: &[String], mime: &str) -> Option<PathBuf> {
    for name in candidate_names(mime) {
        for theme in themes {
            for root in roots {
                for size in SIZES {
                    for context in CONTEXTS {
                        for extension in EXTENSIONS {
                            let path = root
                                .join(theme)
                                .join(size)
                                .join(context)
                                .join(format!("{name}.{extension}"));
                            if path.is_file() {
                                return Some(path);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// The icon names to try for `mime`, most specific first: the MIME type
/// with `/` and `.` replaced by `-`, which is how themes name type icons
/// (`application/x-blender` → `application-x-blender`), then the family
/// glyph for its top level and kind.
fn candidate_names(mime: &str) -> Vec<String> {
    let mut names = vec![mime.replace('/', "-").replace('.', "-")];
    names.extend(family_names(mime).iter().map(|name| (*name).to_owned()));
    names
}

/// The family glyphs for `mime`'s top level and kind: one `-x-generic`
/// per top level, with `text-x-script` before `text-x-generic` for shell
/// scripts and source code, so a script shows the script glyph before the
/// plain text one.
fn family_names(mime: &str) -> &'static [&'static str] {
    if is_script_or_source(mime) {
        return &["text-x-script", "text-x-generic"];
    }
    match mime.split('/').next().unwrap_or_default() {
        "video" => &["video-x-generic"],
        "image" => &["image-x-generic"],
        "audio" => &["audio-x-generic"],
        "text" => &["text-x-generic"],
        _ => &["application-x-generic"],
    }
}

/// Whether `mime` names a program or program text — the shell-script and
/// source-code types the MIME table covers — rather than a data format:
/// the script glyph is for them.
fn is_script_or_source(mime: &str) -> bool {
    matches!(
        mime,
        "application/x-shellscript"
            | "text/x-python"
            | "text/rust"
            | "text/x-perl"
            | "application/x-ruby"
            | "text/x-lua"
            | "text/javascript"
            | "text/x-typescript"
            | "text/x-nix"
            | "text/x-desktop"
            | "text/x-diff"
    )
}

/// The directory sizes probed under a theme, in order: `scalable` first —
/// its SVGs render sharp at every size — then the fixed sizes, large
/// before small.
const SIZES: [&str; 7] = [
    "scalable", "256x256", "128x128", "64x64", "48x48", "32x32", "24x24",
];
/// The contexts probed under a size: type icons live in `mimetypes`,
/// application icons in `apps`, and some themes also file type glyphs
/// under `places`.
const CONTEXTS: [&str; 3] = ["mimetypes", "apps", "places"];
/// The file extensions probed, SVG first for the same reason as
/// [`SIZES`]'s `scalable`.
const EXTENSIONS: [&str; 2] = ["svg", "png"];
/// The themes tried after the configured one and its chain: the ones
/// distributions install with MIME icons, ending in the spec's mandatory
/// `hicolor`.
const FALLBACK_THEMES: [&str; 4] = ["Adwaita", "breeze", "Papirus", "hicolor"];

/// The themes to try, in order: the configured theme, then everything it
/// inherits — each theme's `index.theme` `Inherits=`, followed
/// recursively, cycles cut — then the fixed fallbacks. Each theme appears
/// once.
fn theme_order(roots: &[PathBuf], configured: Option<&str>) -> Vec<String> {
    let mut themes = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue = configured
        .map(str::to_owned)
        .into_iter()
        .collect::<VecDeque<_>>();
    while let Some(theme) = queue.pop_front() {
        if theme.is_empty() || !seen.insert(theme.clone()) {
            continue;
        }
        themes.push(theme.clone());
        queue.extend(inherited(roots, &theme));
    }
    for fallback in FALLBACK_THEMES {
        if seen.insert(fallback.to_owned()) {
            themes.push(fallback.to_owned());
        }
    }
    themes
}

/// The theme GTK 3 is configured to draw icons from:
/// `gtk-icon-theme-name` in `$XDG_CONFIG_HOME/gtk-3.0/settings.ini`
/// (fallback `~/.config/gtk-3.0/settings.ini`). `None` when unset,
/// unreadable or empty.
fn configured_theme() -> Option<String> {
    let settings = config_home()?.join("gtk-3.0").join("settings.ini");
    let text = std::fs::read_to_string(settings).ok()?;
    ini_value(&text, "Settings", "gtk-icon-theme-name")
}

/// `$XDG_CONFIG_HOME` when it is absolute, else `~/.config`.
fn config_home() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_CONFIG_HOME") {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return Some(path);
        }
    }
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".config"))
}

/// The themes `theme` inherits from, in declared order: the `Inherits=`
/// of the first `index.theme` a root has for it. A theme without one
/// inherits nothing.
fn inherited(roots: &[PathBuf], theme: &str) -> Vec<String> {
    for root in roots {
        let Ok(text) = std::fs::read_to_string(root.join(theme).join("index.theme")) else {
            continue;
        };
        return ini_value(&text, "Icon Theme", "Inherits")
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|parent| !parent.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
    }
    Vec::new()
}

/// One `key = value` of `group` in an INI file — GTK's `settings.ini` and
/// the themes' `index.theme` both are — unquoted, or `None` when the group
/// or the key is absent. The first match wins; comments and other groups
/// are skipped.
fn ini_value(text: &str, group: &str, key: &str) -> Option<String> {
    let mut in_group = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_group = line[1..line.len() - 1].trim() == group;
            continue;
        }
        if !in_group {
            continue;
        }
        let Some((found, value)) = line.split_once('=') else {
            continue;
        };
        if found.trim() == key {
            let value = value.trim().trim_matches('"');
            return (!value.is_empty()).then(|| value.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A scratch directory under the temporary directory, unique per
    /// process, empty at the start and removed by the test that made it.
    fn scratch(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("fileflux-icons-test-{tag}-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&path));
        std::fs::create_dir_all(&path).expect("scratch directory");
        path
    }

    /// One icon file inside a theme, creating its directories.
    fn write_icon(
        root: &Path,
        theme: &str,
        size: &str,
        context: &str,
        name: &str,
        extension: &str,
    ) -> PathBuf {
        let directory = root.join(theme).join(size).join(context);
        std::fs::create_dir_all(&directory).expect("theme directory");
        let path = directory.join(format!("{name}.{extension}"));
        std::fs::write(&path, b"icon bytes").expect("icon file");
        path
    }

    /// A theme's `index.theme`, with an `Inherits=` chain.
    fn write_index(root: &Path, theme: &str, inherits: &str) {
        std::fs::create_dir_all(root.join(theme)).expect("theme directory");
        std::fs::write(
            root.join(theme).join("index.theme"),
            format!("[Icon Theme]\nName={theme}\nInherits={inherits}\n"),
        )
        .expect("index.theme");
    }

    /// A theme that has the MIME-specific icon resolves it: a `.blend` file
    /// gets the theme's Blender icon.
    #[test]
    fn a_mime_specific_icon_resolves() {
        let root = scratch("mime-icon");
        let icon = write_icon(
            &root,
            "hicolor",
            "48x48",
            "mimetypes",
            "application-x-blender",
            "png",
        );
        assert_eq!(
            for_file_in(
                &[root.clone()],
                &["hicolor".to_owned()],
                OsStr::new("scene.blend")
            ),
            Some(icon),
            "a Blender file gets the theme's Blender icon"
        );
        drop(std::fs::remove_dir_all(&root));
    }

    /// A family glyph covers the types the theme does not name itself: a
    /// video resolves to `video-x-generic`.
    #[test]
    fn a_family_glyph_covers_what_the_theme_does_not_name() {
        let root = scratch("family");
        let icon = write_icon(
            &root,
            "hicolor",
            "48x48",
            "mimetypes",
            "video-x-generic",
            "png",
        );
        assert_eq!(
            for_file_in(
                &[root.clone()],
                &["hicolor".to_owned()],
                OsStr::new("clip.mp4")
            ),
            Some(icon),
            "a video with no specific icon gets the family glyph"
        );
        drop(std::fs::remove_dir_all(&root));
    }

    /// The `scalable` SVG wins over the same icon as a `48x48` PNG: an SVG
    /// renders sharp at every size.
    #[test]
    fn the_scalable_svg_wins_over_the_same_icon_as_a_png() {
        let root = scratch("scalable");
        write_icon(
            &root,
            "hicolor",
            "48x48",
            "mimetypes",
            "application-x-blender",
            "png",
        );
        let svg = write_icon(
            &root,
            "hicolor",
            "scalable",
            "mimetypes",
            "application-x-blender",
            "svg",
        );
        assert_eq!(
            for_file_in(
                &[root.clone()],
                &["hicolor".to_owned()],
                OsStr::new("scene.blend")
            ),
            Some(svg),
            "the scalable SVG renders sharp at every size, so it is preferred"
        );
        drop(std::fs::remove_dir_all(&root));
    }

    /// A type no theme names answers `None`, and a name with no MIME type
    /// never probes at all: the caller keeps its bundled glyph.
    #[test]
    fn a_type_no_theme_names_has_no_icon() {
        let root = scratch("none");
        write_icon(
            &root,
            "hicolor",
            "48x48",
            "mimetypes",
            "video-x-generic",
            "png",
        );
        assert_eq!(
            for_file_in(
                &[root.clone()],
                &["hicolor".to_owned()],
                OsStr::new("notes.txt")
            ),
            None,
            "no candidate name exists"
        );
        assert_eq!(
            for_file_in(
                &[root.clone()],
                &["hicolor".to_owned()],
                OsStr::new("Makefile")
            ),
            None,
            "a name with no MIME type never probes at all"
        );
        drop(std::fs::remove_dir_all(&root));
    }

    /// The configured theme wins over `hicolor` when both have the icon.
    #[test]
    fn the_configured_theme_wins_over_hicolor() {
        let root = scratch("priority");
        write_icon(
            &root,
            "hicolor",
            "48x48",
            "mimetypes",
            "application-x-blender",
            "png",
        );
        let configured = write_icon(
            &root,
            "Papirus",
            "48x48",
            "mimetypes",
            "application-x-blender",
            "png",
        );
        assert_eq!(
            for_file_in(
                &[root.clone()],
                &["Papirus".to_owned(), "hicolor".to_owned()],
                OsStr::new("scene.blend")
            ),
            Some(configured),
            "the theme list is tried in order, the configured theme first"
        );
        drop(std::fs::remove_dir_all(&root));
    }

    /// The theme list follows the configured theme and its `Inherits=`
    /// chain, then the fixed fallbacks; without a configured theme the
    /// fallbacks alone apply, and a cycle is cut, not followed forever.
    #[test]
    fn the_theme_list_follows_the_configured_theme_and_its_inherits_chain() {
        let root = scratch("chain");
        write_index(&root, "Custom", "Mid,hicolor");
        write_index(&root, "Mid", "");
        write_index(&root, "Loop", "Loop,Custom");
        assert_eq!(
            theme_order(&[root.clone()], Some("Custom")),
            ["Custom", "Mid", "hicolor", "Adwaita", "breeze", "Papirus"].map(str::to_owned),
            "the configured theme, its chain, then the fixed fallbacks"
        );
        assert_eq!(
            theme_order(&[root.clone()], None),
            ["Adwaita", "breeze", "Papirus", "hicolor"].map(str::to_owned),
            "without a configured theme the fallbacks alone apply"
        );
        assert_eq!(
            theme_order(&[root.clone()], Some("Loop")),
            [
                "Loop", "Custom", "Mid", "hicolor", "Adwaita", "breeze", "Papirus"
            ]
            .map(str::to_owned),
            "a cycle is cut, not followed forever"
        );
        drop(std::fs::remove_dir_all(&root));
    }
}
