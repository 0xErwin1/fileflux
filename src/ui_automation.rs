//! The optional UI automation bridge: with the `ui-automation` feature a
//! client running as the same user can find the window, read the element tree
//! it rendered and inject input, through `gpui-mcp`.
//!
//! This is a development tool. The feature is off by default, so a release
//! build links neither the bridge nor its dependencies, and every window may
//! call [`install`] unconditionally: the gate lives here.

use gpui::{App, Window};

/// Installs the automation bridge into `window`.
///
/// The bridge lives until the window closes or the application quits, when its
/// listener stops and its discovery descriptor is removed. A failure is logged
/// and leaves the window usable without automation.
#[cfg(feature = "ui-automation")]
pub(crate) fn install(window: &mut Window, cx: &mut App) {
    bridge::install(window, cx);
}

/// Nothing: the `ui-automation` feature is disabled.
#[cfg(not(feature = "ui-automation"))]
pub(crate) fn install(_window: &mut Window, _cx: &mut App) {}

#[cfg(feature = "ui-automation")]
mod bridge {
    use std::collections::HashMap;

    use gpui::{App, Global, Window, WindowId};
    use gpui_mcp::{AppId, BridgeConfig, BridgeHandle};

    /// The application id the automation client selects the window by:
    /// `gpui-mcp --app-id fileflux`.
    const APP_ID: &str = "fileflux";
    /// The name the client shows for this application.
    const APP_NAME: &str = "Fileflux";

    /// The bridge handles, by the window they serve. A handle stops its
    /// listener when dropped, so the foreground thread owns them for as long
    /// as their window exists.
    #[derive(Default)]
    struct Bridges(HashMap<WindowId, BridgeHandle>);

    impl Global for Bridges {}

    pub(super) fn install(window: &mut Window, cx: &mut App) {
        let app_id = match AppId::new(APP_ID) {
            Ok(app_id) => app_id,
            Err(error) => {
                log::warn!("UI automation bridge not installed: {error}");
                return;
            }
        };
        let config = BridgeConfig::new(app_id, APP_NAME);
        let handle = match BridgeHandle::install(window, cx, config) {
            Ok(handle) => handle,
            Err(error) => {
                log::warn!("UI automation bridge not installed: {error}");
                return;
            }
        };
        log::info!(
            "UI automation bridge listening, descriptor {}",
            handle.endpoint_path().display()
        );
        let window_id = window.window_handle().window_id();
        registry(cx).0.insert(window_id, handle);
    }

    /// The bridges of this process, with the cleanup hooks installed once: a
    /// closed window stops its bridge, and quitting stops them all, so no
    /// discovery descriptor outlives the process.
    fn registry(cx: &mut App) -> &mut Bridges {
        if !cx.has_global::<Bridges>() {
            cx.on_window_closed(|cx, window_id| {
                if let Some(handle) = cx.default_global::<Bridges>().0.remove(&window_id) {
                    handle.shutdown();
                }
            })
            .detach();
            cx.on_app_quit(|cx| {
                for (_, handle) in cx.default_global::<Bridges>().0.drain() {
                    handle.shutdown();
                }
                async {}
            })
            .detach();
        }
        cx.default_global::<Bridges>()
    }
}
