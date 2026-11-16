//! GUI startup.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use gpui_component::Root;

use crate::setup::SetupScreen;
use crate::theme;

/// The size the setup window opens at, until the file explorer settles its
/// own layout.
const MAIN_WINDOW_SIZE: (f32, f32) = (1024.0, 680.0);

/// Initializes logging, installs the Bolt theme and opens the setup window,
/// running until the last window closes.
pub(crate) fn open() -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let startup_error: Rc<RefCell<Option<anyhow::Error>>> = Rc::default();

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

            if let Err(error) = open_setup_window(cx) {
                error_slot.replace(Some(error));
                cx.quit();
            }
        });

    match startup_error.take() {
        None => std::process::ExitCode::SUCCESS,
        Some(error) => {
            eprintln!("fileflux: failed to open the setup window: {error:#}");
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

/// Installs the theme and opens the setup window wrapped in gpui-component's
/// [`Root`].
fn open_setup_window(cx: &mut App) -> Result<()> {
    theme::install(bolt::palette::PaletteMode::System, cx);

    let bounds = Bounds::centered(
        None,
        size(px(MAIN_WINDOW_SIZE.0), px(MAIN_WINDOW_SIZE.1)),
        cx,
    );
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
        let screen = cx.new(|_| SetupScreen::new());
        cx.new(|cx| Root::new(screen, window, cx))
    })?;

    Ok(())
}
