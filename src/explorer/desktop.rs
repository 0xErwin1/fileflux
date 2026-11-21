//! The desktop integration on the UI side: requests that arrived on the
//! session bus (a second launch, "Show in folder" from another application)
//! are drained on the UI thread and become new tabs.

use std::rc::Rc;

use fileflux::core::Location;
use fileflux::dbus::{Primary, Request};
use gpui::{Context, Global, Window};

use super::actions::PendingSelect;
use super::tabs::Placement;
use super::view::Explorer;

/// The bus side of this process, kept as an app global until the explorer
/// window picks it up.
pub(crate) struct DesktopLink(pub(crate) Rc<Primary>);

impl Global for DesktopLink {}

impl Explorer {
    /// Handles what the bus delivered since the last tick.
    pub(crate) fn pump_desktop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(desktop) = self.desktop.clone() else {
            return;
        };
        while let Some(request) = desktop.try_recv() {
            self.show_requested(request, window, cx);
        }
    }

    /// One tab per requested folder, with the requested entries selected,
    /// then the window is raised. A request without folders only raises it.
    fn show_requested(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        for show in request.targets {
            let before = self.active_id();
            self.open_tab(
                Location::Directory(show.folder),
                Placement::Foreground,
                window,
                cx,
            );
            // A refused folder (the Trash) opens nothing: only a new tab
            // gets the selection.
            if self.active_id() != before && !show.select.is_empty() {
                self.tab_mut().pending = Some(PendingSelect {
                    paths: show.select,
                    rename: false,
                });
            }
        }
        if request.properties {
            self.inspector_open = true;
        }
        window.activate_window();
        cx.notify();
    }
}
