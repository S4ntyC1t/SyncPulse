//! Standalone Linux service windows built directly on GTK + WebKitGTK.
//!
//! Tauri/wry build their own `WebKitWebContext` and give SyncPulse no way to
//! configure it: `WebKitWebContext:memory-pressure-settings` is a
//! *construct-only* property. Hosting the window on GTK directly lets SyncPulse
//! hand WebKit a `WebContext` it controls end to end — an explicit memory budget
//! (`webkit_governor`), a capped cache model, and shed feature areas — and drops
//! the whole Tauri/tao/wry runtime from the service process, which otherwise
//! loads a second copy of the app machinery just to show one web page.
//!
//! Everything Tauri used to provide is reproduced here: the desktop application
//! ID (Wayland `app_id` / `StartupWMClass`), the window icon, the per-service
//! initialization scripts, MediaSession handling, and opening external links in
//! the user's real browser instead of a bare WebView.

#![cfg(target_os = "linux")]

use std::path::PathBuf;

use crate::webkit_governor::{self, WebKitRole};
use crate::ServiceWindow;
use gtk::prelude::*;
use gtk::{gdk_pixbuf, gio};
use webkit2gtk::{
    URIRequestExt, UserContentInjectedFrames, UserContentManager, UserContentManagerExt,
    UserScript, UserScriptInjectionTime, WebContext, WebView, WebViewExt, WebsiteDataManager,
};

/// Run the standalone service window. Blocks on the GTK main loop until the
/// window is closed, then returns.
pub(crate) fn run(service: ServiceWindow, url: url::Url) -> Result<(), String> {
    // Must happen before GTK initializes so GDK picks the service-specific
    // process name for the Wayland toplevel.
    crate::desktop::set_linux_application_id(service.id());
    crate::webview_compat::apply_linux_webview_env();

    gtk::init().map_err(|error| {
        format!(
            "Unable to initialize GTK for the {} window: {error}",
            service.title()
        )
    })?;

    let application = gtk::Application::new(None, gio::ApplicationFlags::empty());
    let url = url.to_string();
    application.connect_activate(move |application| {
        if let Err(error) = build_window(application, service, &url) {
            tracing::error!("Unable to build the {} window: {error}", service.title());
            application.quit();
        }
    });
    // GApplication would otherwise try to parse SyncPulse's own
    // `--syncpulse-service ...` arguments as files/options and abort. Hand it a
    // bare argv so the service window never sees them again.
    application.run_with_args(&["navidrome-companion"]);

    Ok(())
}

fn build_window(
    application: &gtk::Application,
    service: ServiceWindow,
    url: &str,
) -> Result<(), String> {
    let context = build_context(service)?;
    let webview = WebView::builder().web_context(&context).build();
    let settings = WebViewExt::settings(&webview)
        .ok_or_else(|| format!("Unable to read WebKit settings for {}", service.title()))?;
    webkit_governor::apply_settings(&settings, WebKitRole::Service);

    install_initialization_script(&webview, service);
    open_external_links_in_browser(&webview);
    if service == ServiceWindow::Invidious {
        match crate::webview_compat::disable_webkit_features(
            &webview,
            &["MediaSession", "RemoteMediaSessionManager"],
        ) {
            Ok(disabled) if !disabled.is_empty() => {
                tracing::info!(
                    "Disabled WebKit features for the Invidious window: {}",
                    disabled.join(", ")
                );
            }
            Ok(_) => {}
            Err(error) => tracing::warn!("Unable to tune the Invidious WebView: {error}"),
        }
    }

    let window = gtk::ApplicationWindow::new(application);
    window.set_title(service.title());
    window.set_default_size(1100, 760);
    window.set_size_request(720, 520);
    window.set_icon(Some(&service_icon(service)?));
    window.add(&webview);
    window.show_all();
    webview.grab_focus();

    webview.load_uri(url);
    Ok(())
}

/// Build the governed `WebContext` for a service window.
///
/// The data directory intentionally matches the one Tauri used for this service
/// (`$XDG_DATA_HOME/<application id>`), so existing Navidrome/Invidious
/// sessions, cookies, and local storage survive the switch to the native host.
fn build_context(service: ServiceWindow) -> Result<WebContext, String> {
    let data_dir = service_data_dir(service);
    std::fs::create_dir_all(&data_dir).map_err(|error| {
        format!(
            "Unable to create the {} data directory {}: {error}",
            service.title(),
            data_dir.display()
        )
    })?;
    let data_dir = data_dir.to_string_lossy().to_string();

    let data_manager = WebsiteDataManager::builder()
        .base_cache_directory(&data_dir)
        .base_data_directory(&data_dir)
        .build();

    let memory = webkit_governor::memory_pressure_settings(WebKitRole::Service);
    let context = WebContext::builder()
        .website_data_manager(&data_manager)
        .memory_pressure_settings(&memory)
        .build();
    webkit_governor::apply_context(&context, WebKitRole::Service);
    Ok(context)
}

/// `$XDG_DATA_HOME/<service application id>`, the same base directory Tauri
/// used for a service webview's website data.
fn service_data_dir(service: ServiceWindow) -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        });
    base.join(service.id())
}

fn service_icon(service: ServiceWindow) -> Result<gdk_pixbuf::Pixbuf, String> {
    gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(service.icon()))
        .map_err(|error| format!("Unable to load the {} icon: {error}", service.title()))
}

fn install_initialization_script(webview: &WebView, service: ServiceWindow) {
    let source = match service {
        ServiceWindow::Navidrome => {
            crate::webview_compat::navidrome_paused_volume_workaround_script()
        }
        ServiceWindow::Invidious => crate::webview_compat::service_window_init_script(),
    };
    let manager: UserContentManager = match webview.user_content_manager() {
        Some(manager) => manager,
        None => {
            tracing::warn!("WebKit did not provide a user content manager");
            return;
        }
    };
    let script = UserScript::new(
        source,
        UserContentInjectedFrames::TopFrame,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    );
    manager.add_script(&script);
}

/// Send `target="_blank"` / `window.open` requests to the user's browser instead
/// of silently dropping them, which is what a bare WebView would do.
fn open_external_links_in_browser(webview: &WebView) {
    webview.connect_create(|_webview, action| {
        if let Some(uri) = action.request().and_then(|request| request.uri()) {
            let uri = uri.to_string();
            if uri.starts_with("http://") || uri.starts_with("https://") {
                if let Err(error) = crate::browser::open(&uri) {
                    tracing::warn!("Unable to open {uri} in the browser: {error}");
                }
            }
        }
        // Never hand back a WebView: every SyncPulse service window is a single
        // page, and extra WebViews would each hold their own WebKit processes.
        None
    });
}
