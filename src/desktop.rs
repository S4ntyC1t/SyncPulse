//! Linux desktop entries and icons for SyncPulse and its WebView service shells.
//!
//! Wayland compositors match windows to icons through the GTK application ID /
//! StartupWMClass. Each service window runs as its own process with a distinct
//! identifier (`org.navidromehub.syncpulse.navidrome`, …), so registering the
//! matching `.desktop` files and hicolor icons makes the taskbar show Navidrome
//! / Invidious instead of a generic Wayland placeholder.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SYNC_PULSE_ID: &str = "org.navidromehub.syncpulse";
const NAVIDROME_ID: &str = "org.navidromehub.syncpulse.navidrome";
const INVIDIOUS_ID: &str = "org.navidromehub.syncpulse.invidious";
const NAVIDROME_ICON: &str = "syncpulse-navidrome-v2";
const INVIDIOUS_ICON: &str = "syncpulse-invidious-v2";

/// Set the process name before GTK initializes. On GTK 3, Tao's GTK application
/// ID is not necessarily propagated to GDK's Wayland `xdg_toplevel.app_id`;
/// GDK falls back to the process name, which otherwise becomes the executable
/// name (`navidrome-companion`) and KDE displays the generic Wayland icon.
pub fn set_linux_application_id(application_id: &str) {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;

        #[link(name = "glib-2.0")]
        unsafe extern "C" {
            fn g_set_prgname(prgname: *const std::ffi::c_char);
        }

        if let Ok(application_id) = CString::new(application_id) {
            // GTK/GDK reads this process-global value during initialization.
            unsafe { g_set_prgname(application_id.as_ptr()) };
        }
    }

    #[cfg(not(target_os = "linux"))]
    let _ = application_id;
}

/// Install or refresh user-local desktop entries and icons for the current
/// SyncPulse binary. Safe to call on every launch; failures are non-fatal.
pub fn ensure_linux_desktop_integration(
    executable: &Path,
    navidrome_port: u16,
    invidious_port: u16,
) -> Result<(), String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (executable, navidrome_port, invidious_port);
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        let data_home = data_home();
        let applications = data_home.join("applications");
        let icons = data_home
            .join("icons")
            .join("hicolor")
            .join("256x256")
            .join("apps");
        fs::create_dir_all(&applications)
            .map_err(|error| format!("Unable to create {}: {error}", applications.display()))?;
        fs::create_dir_all(&icons)
            .map_err(|error| format!("Unable to create {}: {error}", icons.display()))?;

        write_icon(
            &icons.join(format!("{SYNC_PULSE_ID}.png")),
            include_bytes!("../icons/syncpulse.png"),
        )?;
        write_icon(
            &icons.join(format!("{NAVIDROME_ICON}.png")),
            include_bytes!("../icons/navidrome.png"),
        )?;
        write_icon(
            &icons.join(format!("{INVIDIOUS_ICON}.png")),
            include_bytes!("../icons/invidious.png"),
        )?;

        let exe = shell_quote(&executable.display().to_string());
        write_desktop(
            &applications.join(format!("{SYNC_PULSE_ID}.desktop")),
            &format!(
                "[Desktop Entry]\n\
Type=Application\n\
Name=SyncPulse\n\
Comment=Install and manage self-hosted open-source services\n\
Exec={exe}\n\
Icon={SYNC_PULSE_ID}\n\
Terminal=false\n\
Categories=Utility;\n\
StartupWMClass={SYNC_PULSE_ID}\n\
StartupNotify=true\n"
            ),
        )?;
        write_desktop(
            &applications.join(format!("{NAVIDROME_ID}.desktop")),
            &format!(
                "[Desktop Entry]\n\
Type=Application\n\
Name=Navidrome\n\
Comment=Open the local Navidrome music server in its SyncPulse WebView shell\n\
Exec={exe} --syncpulse-service navidrome configured {navidrome_port}\n\
Icon={NAVIDROME_ICON}\n\
Terminal=false\n\
Categories=AudioVideo;Audio;Player;\n\
StartupWMClass={NAVIDROME_ID}\n\
StartupNotify=true\n"
            ),
        )?;
        write_desktop(
            &applications.join(format!("{INVIDIOUS_ID}.desktop")),
            &format!(
                "[Desktop Entry]\n\
Type=Application\n\
Name=Invidious\n\
Comment=Open the local Invidious video service in its SyncPulse WebView shell\n\
Exec={exe} --syncpulse-service invidious configured {invidious_port}\n\
Icon={INVIDIOUS_ICON}\n\
Terminal=false\n\
Categories=AudioVideo;Video;Player;\n\
StartupWMClass={INVIDIOUS_ID}\n\
StartupNotify=true\n"
            ),
        )?;

        refresh_desktop_caches(&applications);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        })
}

#[cfg(target_os = "linux")]
fn write_icon(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if path.is_file() {
        if let Ok(existing) = fs::read(path) {
            if existing == bytes {
                return Ok(());
            }
        }
    }
    fs::write(path, bytes).map_err(|error| format!("Unable to write {}: {error}", path.display()))
}

#[cfg(target_os = "linux")]
fn write_desktop(path: &Path, contents: &str) -> Result<(), String> {
    if path.is_file() {
        if let Ok(existing) = fs::read_to_string(path) {
            if existing == contents {
                return Ok(());
            }
        }
    }
    fs::write(path, contents)
        .map_err(|error| format!("Unable to write {}: {error}", path.display()))
}

#[cfg(target_os = "linux")]
fn shell_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(target_os = "linux")]
fn refresh_desktop_caches(applications: &Path) {
    let _ = Command::new("update-desktop-database")
        .arg(applications)
        .status();
    if Command::new("kbuildsycoca6")
        .arg("--noincremental")
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
    {
        return;
    }
    let _ = Command::new("kbuildsycoca5")
        .arg("--noincremental")
        .status();
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn shell_quote_escapes_embedded_quotes() {
        assert_eq!(super::shell_quote(r#"/tmp/a "b""#), r#""/tmp/a \"b\"""#);
    }
}
