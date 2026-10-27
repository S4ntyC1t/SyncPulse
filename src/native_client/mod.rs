//! A native Navidrome client: Navidrome's own library, drawn with GTK widgets.
//!
//! SyncPulse can host Navidrome's web interface in a WebKit window
//! (`service_window`), but that costs a full `WebKitWebProcess` plus the React
//! application's heap — several hundred megabytes to show a music library. This
//! module instead talks to Navidrome's Subsonic API and draws the library with
//! plain GTK widgets, which is an order of magnitude cheaper and keeps the
//! player fully under SyncPulse's control (global media keys, Discord presence,
//! the tray) instead of inside a page.
//!
//! Layout: submodules by concern — [`api`] (Subsonic), [`player`] (mpv),
//! [`ui`] (widgets, navigation, cover loading) and [`views`] (page builders).
//!
//! Launch with `navidrome-companion --syncpulse-client`.

#![cfg(target_os = "linux")]

mod api;
mod player;
mod ui;
mod views;

/// Wayland application ID / `StartupWMClass` for the client window.
pub(crate) const CLIENT_APP_ID: &str = "org.navidromehub.syncpulse.client";

/// Run the native client. Blocks on the GTK main loop until the window closes.
pub(crate) fn run() -> Result<(), String> {
    // Must happen before GTK initializes so GDK picks this process name for the
    // Wayland toplevel and KWin associates the window with SyncPulse.
    crate::desktop::set_linux_application_id(CLIENT_APP_ID);
    ui::run()
}
