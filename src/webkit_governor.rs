//! SyncPulse's own WebKitGTK memory governor.
//!
//! WebKitGTK ships a memory-pressure monitor, but by default it only listens for
//! *system-wide* low-memory signals. On a desktop with plenty of free RAM it
//! therefore never acts, and a rendered web app (Navidrome's React player,
//! Invidious) quietly accumulates a grown JavaScript heap and decoded resources
//! until its `WebKitWebProcess` holds several hundred megabytes.
//!
//! The governor gives every WebKit process an explicit budget instead. WebKit
//! then polls its own footprint and releases memory (garbage collection plus
//! cache eviction) as soon as the process crosses the configured fraction of
//! that budget, independent of the rest of the machine. The budget scales with
//! installed RAM, and the thresholds are kept far enough below it that WebKit
//! *trims* the process rather than ever reaching the kill threshold (a killed
//! page would drop playback and silently reload).
//!
//! SyncPulse additionally sheds WebKit feature areas its windows never use and
//! caps the network cache model, both of which cost memory for no benefit.

#![cfg(target_os = "linux")]

use webkit2gtk::{
    CacheModel, MemoryPressureSettings, Settings, SettingsExt, WebContext, WebContextExt,
};

/// How a WebKit window is used; drives how aggressively it is kept in check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WebKitRole {
    /// SyncPulse's own control UI: a static local page that never plays media.
    ControlUi,
    /// A service shell rendering a third-party web app (Navidrome/Invidious).
    Service,
}

/// A pure, testable description of the memory budget for one WebKit process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MemoryProfile {
    pub memory_limit_mb: u32,
    pub conservative_threshold: f64,
    pub strict_threshold: f64,
    pub kill_threshold: f64,
    pub poll_interval_secs: f64,
}

impl MemoryProfile {
    /// Derive the profile for a role from the installed amount of RAM.
    ///
    /// `memory_limit_mb` is the budget the monitor measures the process against.
    /// The thresholds are kept far apart from the kill point so WebKit *trims*
    /// the process (garbage collection plus cache eviction) long before it would
    /// ever terminate it — a killed page would drop playback and silently
    /// reload the web app.
    pub(crate) fn for_role(role: WebKitRole, total_mb: u64) -> Self {
        let total_mb = total_mb.max(1) as f64;
        let (fraction, floor, ceiling) = match role {
            WebKitRole::ControlUi => (0.04, 384.0, 1024.0),
            WebKitRole::Service => (0.06, 768.0, 2048.0),
        };
        Self {
            memory_limit_mb: (total_mb * fraction).clamp(floor, ceiling) as u32,
            conservative_threshold: 0.5,
            strict_threshold: 0.75,
            kill_threshold: 0.97,
            // Poll our own footprint instead of waiting for a system-wide
            // low-memory event, which on a roomy desktop never arrives.
            poll_interval_secs: 4.0,
        }
    }
}

/// Build the WebKit memory-pressure settings for a role.
pub(crate) fn memory_pressure_settings(role: WebKitRole) -> MemoryPressureSettings {
    let profile = MemoryProfile::for_role(role, total_memory_mb().unwrap_or(4096));
    let mut settings = MemoryPressureSettings::new();
    settings.set_memory_limit(profile.memory_limit_mb);
    // Order matters: each threshold setter asserts it still satisfies
    // `conservative < strict < kill` against the *current* other values, so the
    // strict threshold has to be raised before the conservative one.
    settings.set_strict_threshold(profile.strict_threshold);
    settings.set_conservative_threshold(profile.conservative_threshold);
    settings.set_kill_threshold(profile.kill_threshold);
    settings.set_poll_interval(profile.poll_interval_secs);
    settings
}

/// Cap the disk/network cache for a window's context.
///
/// Navidrome and Invidious both talk to a loopback server, so a large network
/// cache buys nothing; the service model keeps a small cache for the current
/// document. The control UI is a single static page, so it can run with the
/// document-viewer model, which keeps essentially nothing.
pub(crate) fn apply_context(context: &WebContext, role: WebKitRole) {
    context.set_cache_model(match role {
        WebKitRole::ControlUi => CacheModel::DocumentViewer,
        WebKitRole::Service => CacheModel::DocumentBrowser,
    });
}

/// Shed WebKit feature areas the given role never uses.
pub(crate) fn apply_settings(settings: &Settings, role: WebKitRole) {
    // The back/forward page cache keeps whole rendered pages alive. All of
    // SyncPulse's windows are single-page apps that never navigate back.
    settings.set_enable_page_cache(false);
    settings.set_enable_developer_extras(false);
    // No SyncPulse window draws with WebGL, and a live GL context costs GPU and
    // main memory on top of the process footprint.
    settings.set_enable_webgl(false);
    if role == WebKitRole::ControlUi {
        // The control UI has no audio and no media elements at all.
        settings.set_enable_webaudio(false);
        settings.set_enable_mediasource(false);
        settings.set_enable_media_stream(false);
    }
}

/// Apply the governor to an already-created WebView (settings + context).
///
/// This is the path for the main window: Tauri owns its `WebContext`, so the
/// memory budget cannot be injected, but the cache model and feature set can.
pub(crate) fn tune_webview(webview: &webkit2gtk::WebView, role: WebKitRole) {
    use webkit2gtk::WebViewExt;

    if let Some(settings) = webview.settings() {
        apply_settings(&settings, role);
    }
    if let Some(context) = webview.context() {
        apply_context(&context, role);
    }
}

/// Total physical memory in MiB, read straight from `/proc/meminfo` so the
/// governor does not need a `sysinfo` refresh just to size a budget.
fn total_memory_mb() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{total_memory_mb, MemoryProfile, WebKitRole};

    fn profile(role: WebKitRole, total_mb: u64) -> MemoryProfile {
        MemoryProfile::for_role(role, total_mb)
    }

    #[test]
    fn service_budget_is_larger_than_the_control_ui_budget() {
        let service = profile(WebKitRole::Service, 16_384);
        let ui = profile(WebKitRole::ControlUi, 16_384);
        assert!(service.memory_limit_mb > ui.memory_limit_mb);
    }

    #[test]
    fn thresholds_leave_room_below_the_kill_point() {
        let profile = profile(WebKitRole::Service, 16_384);
        assert!(profile.conservative_threshold < profile.strict_threshold);
        assert!(profile.strict_threshold < profile.kill_threshold);
        assert!(profile.poll_interval_secs > 0.0);
    }

    #[test]
    fn budget_scales_with_installed_memory() {
        // A roomy machine gets a larger budget than a small one.
        assert!(
            profile(WebKitRole::Service, 32_768).memory_limit_mb
                > profile(WebKitRole::Service, 4096).memory_limit_mb
        );
        // And the floor/ceiling always bound it.
        for total in [512, 2048, 4096, 16_384, 131_072] {
            let limits = profile(WebKitRole::Service, total).memory_limit_mb;
            assert!((768..=2048).contains(&limits));
            let ui = profile(WebKitRole::ControlUi, total).memory_limit_mb;
            assert!((384..=1024).contains(&ui));
            // Even the smallest budget stays far above the kill fraction of a
            // realistic Navidrome/Invidious working set.
            assert!(limits >= 768);
        }
    }

    #[test]
    fn installed_memory_is_read_from_proc() {
        // On Linux the governor can always size a budget from /proc/meminfo.
        assert!(total_memory_mb().unwrap_or(0) > 0);
    }
}
