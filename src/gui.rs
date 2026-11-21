//! GUI startup: logging, the Bolt theme (Bolt's existing dark palette) and
//! the explorer or viewer window inside `gpui_component`'s [`Root`], reusing the
//! startup lifecycle the setup screen proved.

use std::borrow::Cow;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Result;
use fileflux::cli::Target;
use gpui::{
    App, AppContext as _, AssetSource, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px,
    size,
};
use gpui_component::Root;

use crate::explorer::persist::Startup;
use crate::explorer::{Explorer, bind_keys};
use crate::theme;
use crate::viewer;

/// Initializes logging; call once, before anything logs.
pub(crate) fn init_logging() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
}

/// Installs the Bolt theme and opens the explorer
/// window at the resolved directory (home when none) or the image viewer,
/// running until the last window closes.
pub(crate) fn open(
    target: Target,
    desktop: Option<fileflux::dbus::Primary>,
) -> std::process::ExitCode {
    let desktop = desktop.map(Rc::new);

    let startup_error: Rc<RefCell<Option<anyhow::Error>>> = Rc::default();

    let window_name = match &target {
        Target::Explorer(_) => "explorer",
        Target::Viewer(_) => "viewer",
    };
    let target = Rc::new(target);
    let error_slot = Rc::clone(&startup_error);
    gpui_platform::application()
        .with_assets(assets())
        .run(move |cx: &mut App| {
            // Quit when the last window closes, instead of running without
            // any window.
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            if let Some(desktop) = &desktop {
                cx.set_global(crate::explorer::desktop::DesktopLink(Rc::clone(desktop)));
            }
            let opened = match &*target {
                Target::Explorer(start) => open_explorer_window(start.clone(), cx),
                Target::Viewer(launch) => viewer::open_window(launch, cx),
            };
            if let Err(error) = opened {
                error_slot.replace(Some(error));
                cx.quit();
            }
        });

    match startup_error.take() {
        None => std::process::ExitCode::SUCCESS,
        Some(error) => {
            eprintln!("fileflux: failed to open the {window_name} window: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The Lucide icons gpui-component bundles plus the installed icon theme's
/// files: relative paths keep coming from the bundled catalog, and a path
/// starting with `/` — an icon `core::icons` resolved on disk — is read
/// from the filesystem, which is what lets a theme SVG render through
/// `svg().path(...)`. Pass it to `application().with_assets`; without it,
/// icons paint nothing.
fn assets() -> impl gpui::AssetSource {
    ThemeAssets
}

/// The asset source behind [`assets`]: everything the bundled Lucide
/// catalog serves, with absolute paths read from disk on top.
#[derive(Clone, Copy, Debug, Default)]
struct ThemeAssets;

impl AssetSource for ThemeAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.starts_with('/') {
            return match std::fs::read(path) {
                Ok(bytes) => Ok(Some(Cow::Owned(bytes))),
                Err(error) => {
                    log::debug!("cannot read the theme icon {path}: {error}");
                    Ok(None)
                }
            };
        }
        gpui_kit_assets::AllAssets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<gpui::SharedString>> {
        gpui_kit_assets::AllAssets.list(path)
    }
}

/// Installs the theme, binds the explorer's keys and opens the explorer
/// window wrapped in gpui-component's [`Root`].
fn open_explorer_window(start: Option<PathBuf>, cx: &mut App) -> Result<()> {
    theme::install(theme::STARTUP_PALETTE, cx);
    bind_keys(cx);

    // The settings and session files are small: read them here, before the
    // window opens, so its size and the first tabs are the saved ones. A
    // path on the command line wins over the saved session.
    let startup = Startup::load(start.as_deref());
    let window_size = (
        startup.preferences.window_width as f32,
        startup.preferences.window_height as f32,
    );
    let bounds = Bounds::centered(None, size(px(window_size.0), px(window_size.1)), cx);
    let options = WindowOptions {
        app_id: Some("fileflux".into()),
        titlebar: Some(TitlebarOptions {
            title: Some(theme::brand().name),
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..Default::default()
    };

    cx.open_window(options, |window, cx| {
        // A development build can be driven by `gpui-mcp`; a release build
        // compiles this to nothing.
        crate::ui_automation::install(window, cx);
        let explorer = cx.new(|cx| Explorer::new(start, startup, window, cx));
        explorer.update(cx, |explorer, cx| {
            explorer.initial_focus().focus(window, cx);
        });
        // Closing the window saves what the debounce has not yet written.
        let closing = explorer.clone();
        window.on_window_should_close(cx, move |window, cx| {
            closing.update(cx, |explorer, _cx| explorer.flush_persistence(window));
            true
        });
        cx.new(|cx| Root::new(explorer, window, cx))
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Absolute paths are read from disk — an icon the theme resolved — a
    /// missing one is no asset rather than an error, and relative paths keep
    /// delegating to the bundled Lucide catalog: its icons still arrive and
    /// its own missing-asset error for unknown names is kept.
    #[test]
    fn theme_assets_read_absolute_paths_and_delegate_the_rest() -> TestResult {
        let scratch =
            std::env::temp_dir().join(format!("fileflux-theme-assets-test-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&scratch));
        std::fs::create_dir_all(&scratch)?;
        let icon = scratch.join("application-x-blender.svg");
        std::fs::write(&icon, b"<svg/>")?;

        let loaded = ThemeAssets.load(&icon.to_string_lossy())?;
        assert_eq!(loaded, Some(Cow::Borrowed(b"<svg/>".as_slice())));
        assert_eq!(
            ThemeAssets.load(&scratch.join("missing.svg").to_string_lossy())?,
            None,
            "an unreadable absolute path is no asset, not an error"
        );

        let bundled = gpui_kit_assets::IconName::File.path();
        assert!(
            ThemeAssets.load(&bundled)?.is_some(),
            "the bundled Lucide catalog still serves its icons"
        );
        assert!(
            ThemeAssets.load("icons/no-such-icon.svg").is_err(),
            "the catalog's own error for unknown names is kept"
        );

        drop(std::fs::remove_dir_all(&scratch));
        Ok(())
    }
}
