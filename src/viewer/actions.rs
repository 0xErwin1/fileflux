//! The viewer's actions and their key bindings.

use gpui::{App, KeyBinding, actions};

/// The key context of the viewer window's root.
pub(crate) const VIEWER_CONTEXT: &str = "FilefluxViewer";

actions!(
    fileflux_viewer,
    [
        /// Closes the viewer window.
        Close,
        /// The previous image in the list.
        Previous,
        /// The next image in the list.
        Next,
        /// The first image of the list.
        First,
        /// The last image of the list.
        Last,
        /// One zoom notch in.
        ZoomIn,
        /// One zoom notch out.
        ZoomOut,
        /// Fits the image to the window.
        FitToWindow,
        /// Shows the image at 100%.
        ZoomActual,
        /// Rotates a quarter turn clockwise.
        RotateRight,
        /// Rotates a quarter turn counter-clockwise.
        RotateLeft,
        /// Shows or hides the info panel.
        ToggleInfo,
        /// Toggles fullscreen.
        ToggleFullscreen,
        /// Opens the containing folder in Fileflux.
        OpenFolder,
    ]
);

/// Binds the viewer's keys; the viewer context keeps them off the explorer.
pub(crate) fn bind_keys(cx: &mut App) {
    let ctx = Some(VIEWER_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("escape", Close, ctx),
        KeyBinding::new("q", Close, ctx),
        KeyBinding::new("left", Previous, ctx),
        KeyBinding::new("pageup", Previous, ctx),
        KeyBinding::new("right", Next, ctx),
        KeyBinding::new("pagedown", Next, ctx),
        KeyBinding::new("home", First, ctx),
        KeyBinding::new("end", Last, ctx),
        KeyBinding::new("+", ZoomIn, ctx),
        KeyBinding::new("=", ZoomIn, ctx),
        KeyBinding::new("shift-=", ZoomIn, ctx),
        KeyBinding::new("ctrl-=", ZoomIn, ctx),
        KeyBinding::new("-", ZoomOut, ctx),
        KeyBinding::new("ctrl--", ZoomOut, ctx),
        KeyBinding::new("0", FitToWindow, ctx),
        KeyBinding::new("ctrl-0", FitToWindow, ctx),
        KeyBinding::new("1", ZoomActual, ctx),
        KeyBinding::new("ctrl-1", ZoomActual, ctx),
        KeyBinding::new("r", RotateRight, ctx),
        KeyBinding::new("shift-r", RotateLeft, ctx),
        KeyBinding::new("i", ToggleInfo, ctx),
        KeyBinding::new("f", ToggleFullscreen, ctx),
        KeyBinding::new("f11", ToggleFullscreen, ctx),
    ]);
}
