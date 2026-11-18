//! GUI startup: logging, the Bolt theme (Bolt's existing dark palette) and
//! the explorer or viewer window inside `gpui_component`'s [`Root`], reusing the
//! startup lifecycle the setup screen proved.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Result;
use fileflux::cli::Target;
use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use gpui_component::Root;

use crate::explorer::persist::Startup;
use crate::explorer::{Explorer, bind_keys};
use crate::theme;
use crate::viewer;

/// Initializes logging, installs the Bolt theme and opens the explorer
/// window at the resolved directory (home when none) or the image viewer,
/// running until the last window closes.
pub(crate) fn open(target: Target) -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

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

/// The Lucide icons gpui-component bundles, which Bolt's `IconSource::Svg`
/// draws. Pass it to `application().with_assets`; without it, icons paint
/// nothing.
fn assets() -> impl gpui::AssetSource {
    gpui_kit_assets::AllAssets
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
