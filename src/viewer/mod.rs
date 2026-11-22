//! The Fileflux image viewer: a window of its own, without the explorer's
//! sidebar, to look at one image at a time and walk through its folder.

mod actions;
mod facts;
mod header;
mod nav;
mod rotation;
mod view;
mod zoom;

use std::path::PathBuf;

use anyhow::Result;
use fileflux::cli::ViewerLaunch;
use gpui::{
    App, AppContext as _, Bounds, Global, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use gpui_component::Root;

use crate::theme;
use view::Viewer;

/// The viewer's typical window: a comfortable 16:10 canvas.
const WINDOW_SIZE: (f32, f32) = (1200.0, 800.0);

/// Marks that the viewer's keys are bound in this app.
struct KeysBound;

impl Global for KeysBound {}

/// Opens a viewer window on `launch`, installing the Bolt theme and the
/// viewer's keys first when nothing else has (the explorer does so itself).
pub(crate) fn open_window(launch: &ViewerLaunch, cx: &mut App) -> Result<()> {
    install_defaults(cx);
    let (list, index) = nav::plan(launch, nav::sibling_images);
    open_on(list, index, cx)
}

fn install_defaults(cx: &mut App) {
    if !cx.has_global::<gpui_component::theme::Theme>() {
        theme::install(theme::STARTUP_PALETTE, cx);
    }
    if !cx.has_global::<KeysBound>() {
        actions::bind_keys(cx);
        cx.set_global(KeysBound);
    }
}

/// Opens a viewer window on `list[index]` that walks `list` in its order:
/// the explorer passes the images as it displays them.
pub(crate) fn open_list(list: Vec<PathBuf>, index: usize, cx: &mut App) -> Result<()> {
    install_defaults(cx);
    open_on(list, index, cx)
}

fn open_on(list: Vec<PathBuf>, index: usize, cx: &mut App) -> Result<()> {
    let bounds = Bounds::centered(None, size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx);
    let options = WindowOptions {
        app_id: Some("fileflux".into()),
        titlebar: Some(TitlebarOptions {
            title: Some(
                fileflux::core::admin::window_title(
                    &theme::brand().name,
                    fileflux::core::owner_helper::is_root(),
                )
                .into(),
            ),
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..Default::default()
    };
    cx.open_window(options, |window, cx| {
        let viewer = cx.new(|cx| Viewer::new(list, index, window, cx));
        viewer.update(cx, |viewer, cx| {
            window.focus(&gpui::Focusable::focus_handle(viewer, cx), cx);
        });
        cx.new(|cx| Root::new(viewer, window, cx))
    })?;
    Ok(())
}
