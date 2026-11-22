//! The icon a file's type has in the installed freedesktop icon theme: the
//! same icons pcmanfm, Nautilus and Thunar draw — Blender's for `.blend`,
//! the script glyph for a shell script, the document glyph for a PDF — so
//! a file looks the way the desktop draws it instead of only getting the
//! extension-derived bundled glyph. The theme has the last word: when it
//! names nothing for the type, [`for_file`] answers `None` and the caller
//! keeps its bundled glyph.
//!
//! An icon is only worth drawing in its colours, and GPUI's own SVG
//! element paints an alpha mask — one single-colour silhouette — so the
//! found file is rasterized here instead: SVGs with the `resvg` the GPUI
//! stack already links, raster icons with the `image` crate, at the size
//! the caller draws (see [`render`]).
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
//! The probe, like rasterizing a found icon, runs on a background thread
//! ([`background`]): a view asks, draws its bundled glyph until the answer
//! is in, and redraws when [`take_announced`] says one arrived.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{LazyLock, Mutex, OnceLock, PoisonError};

use super::mimes;
use super::thumbnailers::data_directories;

/// The icon file for the type of a file named `name`, from the installed
/// freedesktop icon theme, or `None` when the theme names nothing for it
/// (the caller keeps its bundled glyph) **or has not been asked yet**:
/// finding it probes the filesystem, which the UI thread never does. The
/// first question about a type starts the probe on a background thread and
/// answers `None`; [`announce`] tells the view when an answer arrived, and
/// asking again then returns it. Decided from the name's extension alone.
#[must_use]
pub fn for_file(name: &OsStr) -> Option<PathBuf> {
    let mime = mimes::of_name(name)?;
    match MEMO.ask(mime) {
        Ask::Known(found) => found,
        Ask::Resolving => None,
        Ask::Start => {
            let mime = mime.to_owned();
            background(move || {
                let environment = environment();
                let found = resolve_in(&environment.roots, &environment.themes, &mime);
                MEMO.settle(&mime, found);
                announce();
            });
            None
        }
    }
}

/// The memoized answers, keyed by MIME type (which fixes the candidate
/// names).
static MEMO: LazyLock<Memo> = LazyLock::new(Memo::default);

/// What is known about one type's icon.
#[derive(Debug)]
enum Slot {
    /// The probe is running.
    Resolving,
    /// The probe is over; `None` when the theme has no icon for the type.
    Known(Option<PathBuf>),
}

/// What asking the [`Memo`] about a type came to.
#[derive(Debug)]
enum Ask {
    /// The answer: a type the theme does not cover is `Known(None)` too, so
    /// it is probed once, not per file.
    Known(Option<PathBuf>),
    /// Someone else's probe is running.
    Resolving,
    /// Nobody asked yet: the caller probes and [`Memo::settle`]s.
    Start,
}

/// The memo of [`for_file`]: one probe per type, however many tiles ask.
#[derive(Debug, Default)]
struct Memo {
    slots: Mutex<HashMap<String, Slot>>,
}

impl Memo {
    fn ask(&self, mime: &str) -> Ask {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        match slots.get(mime) {
            Some(Slot::Known(found)) => Ask::Known(found.clone()),
            Some(Slot::Resolving) => Ask::Resolving,
            None => {
                slots.insert(mime.to_owned(), Slot::Resolving);
                Ask::Start
            }
        }
    }

    fn settle(&self, mime: &str, found: Option<PathBuf>) {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(mime.to_owned(), Slot::Known(found));
    }
}

/// Runs `job` on the one background thread the icon work shares, in the order
/// jobs were handed in: probing a theme and rasterizing an icon touch the
/// disk and burn CPU, so neither happens while a frame is being built.
pub fn background(job: impl FnOnce() + Send + 'static) {
    type Job = Box<dyn FnOnce() + Send>;
    static WORKER: OnceLock<Option<Sender<Job>>> = OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (sender, jobs) = std::sync::mpsc::channel::<Job>();
        let started = std::thread::Builder::new()
            .name("icons".to_owned())
            .spawn(move || {
                for job in jobs {
                    job();
                }
            });
        match started {
            Ok(_) => Some(sender),
            Err(error) => {
                log::debug!("could not start the icon thread: {error}");
                None
            }
        }
    });
    if let Some(worker) = worker {
        drop(worker.send(Box::new(job)));
    }
}

/// Set when background icon work finished something a view may now draw.
static ANNOUNCED: AtomicBool = AtomicBool::new(false);

/// Says that an icon is ready: the view redraws when it next [`take_announced`]
/// (a burst of icons costs one redraw).
pub fn announce() {
    ANNOUNCED.store(true, Ordering::Release);
}

/// Whether an icon became ready since the last call.
pub fn take_announced() -> bool {
    ANNOUNCED.swap(false, Ordering::AcqRel)
}

/// A themed icon rasterized in full colour, at the size a view draws it:
/// GPUI's own SVG element paints an alpha mask, so every themed icon
/// would become one single-colour silhouette — the colour is rasterized
/// here instead (see [`render`]).
#[derive(Debug)]
pub struct RenderedIcon {
    /// The raster's width in pixels.
    pub width: u32,
    /// The raster's height in pixels.
    pub height: u32,
    /// The pixels: straight (not premultiplied) RGBA, row-major, exactly
    /// `4 × width × height` bytes.
    pub rgba: Vec<u8>,
}

/// The icon file at `path` rasterized in full colour, its longest edge
/// `size` pixels: an SVG is parsed and rendered with the `resvg` the GPUI
/// stack already links and scales freely to the box; a raster icon (PNG,
/// JPG, WebP, BMP) is decoded with the `image` crate and only ever scaled
/// down, never up beyond its own pixels. `None` on any failure — unreadable,
/// unparseable, undecodable — logged at debug level, so the caller keeps
/// its bundled glyph.
#[must_use]
pub fn render(path: &Path, size: u32) -> Option<RenderedIcon> {
    let size = size.max(1);
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            log::debug!("could not read icon {}: {error}", path.display());
            return None;
        }
    };
    match path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("svg") => render_svg(&bytes, size),
        Some("png" | "jpg" | "jpeg" | "webp" | "bmp") => render_raster(&bytes, size),
        _ => {
            log::debug!("unsupported icon format: {}", path.display());
            None
        }
    }
}

/// An SVG icon rasterized with its longest edge `size` pixels, the other
/// edge keeping its aspect (an SVG scales freely, up and down). `resvg`
/// writes premultiplied alpha, un-premultiplied here because GPUI's image
/// pipeline draws straight alpha.
fn render_svg(bytes: &[u8], size: u32) -> Option<RenderedIcon> {
    let tree = match usvg::Tree::from_data(bytes, &usvg::Options::default()) {
        Ok(tree) => tree,
        Err(error) => {
            log::debug!("could not parse an icon SVG: {error}");
            return None;
        }
    };
    let svg = tree.size();
    let longest = svg.width().max(svg.height());
    if !longest.is_finite() || longest <= 0.0 {
        log::debug!("an icon SVG has no drawable size");
        return None;
    }
    // The box in whole pixels: the longest edge is `size` exactly, the
    // other edge rounds to its nearest pixel.
    let scale = size as f32 / longest;
    let width = (svg.width() * scale).round().clamp(1.0, size as f32) as u32;
    let height = (svg.height() * scale).round().clamp(1.0, size as f32) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(
            width as f32 / svg.width(),
            height as f32 / svg.height(),
        ),
        &mut pixmap.as_mut(),
    );
    Some(unpremultiply(pixmap.take(), width, height))
}

/// A raster icon decoded with the `image` crate, fitted inside `size` and
/// never scaled up: a raster icon ships the pixels it wants, so only a
/// larger one is scaled down to fit the box.
fn render_raster(bytes: &[u8], size: u32) -> Option<RenderedIcon> {
    let icon = match image::load_from_memory(bytes) {
        Ok(icon) => icon,
        Err(error) => {
            log::debug!("could not decode an icon image: {error}");
            return None;
        }
    };
    let icon = if icon.width().max(icon.height()) > size {
        icon.thumbnail(size, size)
    } else {
        icon
    };
    let raster = icon.to_rgba8();
    let (width, height) = (raster.width(), raster.height());
    Some(RenderedIcon {
        width,
        height,
        rgba: raster.into_raw(),
    })
}

/// Premultiplied RGBA pixels — what `resvg` writes — as straight RGBA,
/// the alpha the `image` crate produces and GPUI's image pipeline draws.
fn unpremultiply(mut pixels: Vec<u8>, width: u32, height: u32) -> RenderedIcon {
    for [red, green, blue, alpha] in pixels.as_chunks_mut::<4>().0 {
        let alpha = f32::from(*alpha) / 255.0;
        if alpha > 0.0 {
            for channel in [red, green, blue] {
                *channel = (f32::from(*channel) / alpha).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    RenderedIcon {
        width,
        height,
        rgba: pixels,
    }
}

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
    let mut names = vec![mime.replace(['/', '.'], "-")];
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

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// A scratch directory under the temporary directory, unique per
    /// process, empty at the start and removed by the test that made it.
    fn scratch(tag: &str) -> Result<PathBuf, std::io::Error> {
        let path =
            std::env::temp_dir().join(format!("fileflux-icons-test-{tag}-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&path));
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    /// One icon file inside a theme, creating its directories.
    fn write_icon(
        root: &Path,
        theme: &str,
        size: &str,
        context: &str,
        name: &str,
        extension: &str,
    ) -> Result<PathBuf, std::io::Error> {
        let directory = root.join(theme).join(size).join(context);
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(format!("{name}.{extension}"));
        std::fs::write(&path, b"icon bytes")?;
        Ok(path)
    }

    /// A theme's `index.theme`, with an `Inherits=` chain.
    fn write_index(root: &Path, theme: &str, inherits: &str) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(root.join(theme))?;
        std::fs::write(
            root.join(theme).join("index.theme"),
            format!("[Icon Theme]\nName={theme}\nInherits={inherits}\n"),
        )
    }

    /// A theme that has the MIME-specific icon resolves it: a `.blend` file
    /// gets the theme's Blender icon.
    #[test]
    fn a_mime_specific_icon_resolves() -> TestResult {
        let root = scratch("mime-icon")?;
        let icon = write_icon(
            &root,
            "hicolor",
            "48x48",
            "mimetypes",
            "application-x-blender",
            "png",
        )?;
        assert_eq!(
            for_file_in(
                std::slice::from_ref(&root),
                &["hicolor".to_owned()],
                OsStr::new("scene.blend")
            ),
            Some(icon),
            "a Blender file gets the theme's Blender icon"
        );
        drop(std::fs::remove_dir_all(&root));
        Ok(())
    }

    /// A family glyph covers the types the theme does not name itself: a
    /// video resolves to `video-x-generic`.
    #[test]
    fn a_family_glyph_covers_what_the_theme_does_not_name() -> TestResult {
        let root = scratch("family")?;
        let icon = write_icon(
            &root,
            "hicolor",
            "48x48",
            "mimetypes",
            "video-x-generic",
            "png",
        )?;
        assert_eq!(
            for_file_in(
                std::slice::from_ref(&root),
                &["hicolor".to_owned()],
                OsStr::new("clip.mp4")
            ),
            Some(icon),
            "a video with no specific icon gets the family glyph"
        );
        drop(std::fs::remove_dir_all(&root));
        Ok(())
    }

    /// The `scalable` SVG wins over the same icon as a `48x48` PNG: an SVG
    /// renders sharp at every size.
    #[test]
    fn the_scalable_svg_wins_over_the_same_icon_as_a_png() -> TestResult {
        let root = scratch("scalable")?;
        write_icon(
            &root,
            "hicolor",
            "48x48",
            "mimetypes",
            "application-x-blender",
            "png",
        )?;
        let svg = write_icon(
            &root,
            "hicolor",
            "scalable",
            "mimetypes",
            "application-x-blender",
            "svg",
        )?;
        assert_eq!(
            for_file_in(
                std::slice::from_ref(&root),
                &["hicolor".to_owned()],
                OsStr::new("scene.blend")
            ),
            Some(svg),
            "the scalable SVG renders sharp at every size, so it is preferred"
        );
        drop(std::fs::remove_dir_all(&root));
        Ok(())
    }

    /// A type no theme names answers `None`, and a name with no MIME type
    /// never probes at all: the caller keeps its bundled glyph.
    #[test]
    fn a_type_no_theme_names_has_no_icon() -> TestResult {
        let root = scratch("none")?;
        write_icon(
            &root,
            "hicolor",
            "48x48",
            "mimetypes",
            "video-x-generic",
            "png",
        )?;
        assert_eq!(
            for_file_in(
                std::slice::from_ref(&root),
                &["hicolor".to_owned()],
                OsStr::new("notes.txt")
            ),
            None,
            "no candidate name exists"
        );
        assert_eq!(
            for_file_in(
                std::slice::from_ref(&root),
                &["hicolor".to_owned()],
                OsStr::new("Makefile")
            ),
            None,
            "a name with no MIME type never probes at all"
        );
        drop(std::fs::remove_dir_all(&root));
        Ok(())
    }

    /// The configured theme wins over `hicolor` when both have the icon.
    #[test]
    fn the_configured_theme_wins_over_hicolor() -> TestResult {
        let root = scratch("priority")?;
        write_icon(
            &root,
            "hicolor",
            "48x48",
            "mimetypes",
            "application-x-blender",
            "png",
        )?;
        let configured = write_icon(
            &root,
            "Papirus",
            "48x48",
            "mimetypes",
            "application-x-blender",
            "png",
        )?;
        assert_eq!(
            for_file_in(
                std::slice::from_ref(&root),
                &["Papirus".to_owned(), "hicolor".to_owned()],
                OsStr::new("scene.blend")
            ),
            Some(configured),
            "the theme list is tried in order, the configured theme first"
        );
        drop(std::fs::remove_dir_all(&root));
        Ok(())
    }

    /// The pixel at `(x, y)` of `rendered`, `[r, g, b, a]`, or `None`
    /// outside it. Reads the public fields, never the rasterizer.
    fn pixel_at(rendered: &RenderedIcon, x: u32, y: u32) -> Option<[u8; 4]> {
        let row = rendered
            .rgba
            .chunks_exact(rendered.width as usize * 4)
            .nth(y as usize)?;
        row.as_chunks::<4>().0.get(x as usize).copied()
    }

    /// A themed SVG icon rasterizes in colour at the asked size: a solid
    /// red square becomes 32×32 with an opaque red centre pixel — the
    /// colour GPUI's own SVG element, which paints an alpha mask, would
    /// flatten into one single-colour silhouette.
    #[test]
    fn an_svg_icon_rasterizes_in_colour_at_the_asked_size() -> TestResult {
        let root = scratch("render-svg")?;
        let icon = root.join("red.svg");
        std::fs::write(
            &icon,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\"><rect width=\"64\" height=\"64\" fill=\"#ff0000\"/></svg>",
        )?;
        let rendered = render(&icon, 32).ok_or("the SVG rasterizes")?;
        assert_eq!((rendered.width, rendered.height), (32, 32));
        assert_eq!(
            pixel_at(&rendered, 16, 16),
            Some([255, 0, 0, 255]),
            "the centre keeps the theme's own opaque red"
        );
        drop(std::fs::remove_dir_all(&root));
        Ok(())
    }

    /// The asked size is the icon's longest edge and the other edge keeps
    /// its aspect: a 100×40 SVG rendered at 32 is 32×13 (40 · 32⁄100 =
    /// 12.8, rounded).
    #[test]
    fn the_asked_size_is_a_non_square_svg_longest_edge() -> TestResult {
        let root = scratch("render-wide")?;
        let icon = root.join("wide.svg");
        std::fs::write(
            &icon,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"40\"><rect width=\"100\" height=\"40\" fill=\"#00ff00\"/></svg>",
        )?;
        let rendered = render(&icon, 32).ok_or("the SVG rasterizes")?;
        assert_eq!(rendered.width, 32, "the longest edge becomes the size");
        assert_eq!(rendered.height, 13, "the short edge keeps its aspect");
        drop(std::fs::remove_dir_all(&root));
        Ok(())
    }

    /// A raster icon decodes with its own pixels, is never scaled up, and
    /// one larger than the box is scaled down to fit it.
    #[test]
    fn a_png_icon_decodes_and_only_ever_scales_down() -> TestResult {
        let root = scratch("render-png")?;
        let small = root.join("small.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            24,
            24,
            image::Rgba([10, 20, 30, 255]),
        ))
        .save(&small)?;
        let rendered = render(&small, 32).ok_or("the small PNG decodes")?;
        assert_eq!(
            (rendered.width, rendered.height),
            (24, 24),
            "never scaled up"
        );
        assert_eq!(pixel_at(&rendered, 12, 12), Some([10, 20, 30, 255]));

        let wide = root.join("wide.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            48,
            24,
            image::Rgba([40, 50, 60, 255]),
        ))
        .save(&wide)?;
        let rendered = render(&wide, 24).ok_or("the wide PNG decodes")?;
        assert_eq!(
            (rendered.width, rendered.height),
            (24, 12),
            "scaled down to fit"
        );
        assert_eq!(pixel_at(&rendered, 12, 6), Some([40, 50, 60, 255]));
        drop(std::fs::remove_dir_all(&root));
        Ok(())
    }

    /// An icon file that is not there rasterizes to nothing, so the caller
    /// keeps its bundled glyph.
    #[test]
    fn a_missing_icon_file_rasterizes_to_nothing() -> TestResult {
        let root = scratch("render-missing")?;
        assert!(render(&root.join("gone.svg"), 32).is_none());
        drop(std::fs::remove_dir_all(&root));
        Ok(())
    }

    /// The theme list follows the configured theme and its `Inherits=`
    /// chain, then the fixed fallbacks; without a configured theme the
    /// fallbacks alone apply, and a cycle is cut, not followed forever.
    #[test]
    fn the_theme_list_follows_the_configured_theme_and_its_inherits_chain() -> TestResult {
        let root = scratch("chain")?;
        write_index(&root, "Custom", "Mid,hicolor")?;
        write_index(&root, "Mid", "")?;
        write_index(&root, "Loop", "Loop,Custom")?;
        assert_eq!(
            theme_order(std::slice::from_ref(&root), Some("Custom")),
            ["Custom", "Mid", "hicolor", "Adwaita", "breeze", "Papirus"].map(str::to_owned),
            "the configured theme, its chain, then the fixed fallbacks"
        );
        assert_eq!(
            theme_order(std::slice::from_ref(&root), None),
            ["Adwaita", "breeze", "Papirus", "hicolor"].map(str::to_owned),
            "without a configured theme the fallbacks alone apply"
        );
        assert_eq!(
            theme_order(std::slice::from_ref(&root), Some("Loop")),
            [
                "Loop", "Custom", "Mid", "hicolor", "Adwaita", "breeze", "Papirus"
            ]
            .map(str::to_owned),
            "a cycle is cut, not followed forever"
        );
        drop(std::fs::remove_dir_all(&root));
        Ok(())
    }

    #[test]
    fn a_type_is_resolved_once_and_asking_meanwhile_starts_nothing() {
        let memo = Memo::default();
        assert!(matches!(memo.ask("image/png"), Ask::Start));
        assert!(
            matches!(memo.ask("image/png"), Ask::Resolving),
            "a second tile of the type waits for the first one's probe"
        );
        assert!(matches!(memo.ask("video/mp4"), Ask::Start), "another type");
    }

    #[test]
    fn a_settled_answer_is_kept_and_a_type_without_an_icon_is_never_probed_again() {
        let memo = Memo::default();
        assert!(matches!(memo.ask("image/png"), Ask::Start));
        memo.settle("image/png", Some(PathBuf::from("/t/png.svg")));
        assert!(matches!(
            memo.ask("image/png"),
            Ask::Known(Some(found)) if found == Path::new("/t/png.svg")
        ));
        assert!(matches!(memo.ask("application/x-nothing"), Ask::Start));
        memo.settle("application/x-nothing", None);
        for _ in 0..3 {
            assert!(
                matches!(memo.ask("application/x-nothing"), Ask::Known(None)),
                "the miss is remembered: the bundled glyph stays without a new probe"
            );
        }
    }

    #[test]
    fn background_work_runs_off_the_calling_thread_and_announces_itself() -> TestResult {
        // Whatever an earlier test announced is not this one's concern.
        take_announced();
        let (done, received) = std::sync::mpsc::channel();
        let caller = std::thread::current().id();
        background(move || {
            done.send(std::thread::current().id()).ok();
            announce();
        });
        let worker = received.recv_timeout(std::time::Duration::from_secs(5))?;
        assert_ne!(worker, caller, "the job did not run on the UI thread");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !take_announced() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!take_announced(), "the announcement is taken once");
        Ok(())
    }
}
