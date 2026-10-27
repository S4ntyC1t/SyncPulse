use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerEngine {
    Docker,
    Podman,
}

impl Default for ContainerEngine {
    fn default() -> Self {
        Self::Docker
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// False only for a genuinely new profile; older config files are migrated
    /// as already configured so an update never unexpectedly opens onboarding.
    pub setup_completed: bool,
    pub navidrome_port: u16,
    pub invidious_port: u16,
    pub container_engine: ContainerEngine,
    /// `local` is loopback, `lan` binds all interfaces, `tailscale` binds the
    /// device's private Tailscale address only.
    pub network_access: String,
    pub music_dir: PathBuf,
    pub data_dir: PathBuf,
    pub bin_dir: PathBuf,
    pub plugins_dir: PathBuf,
    pub auto_start: bool,
    pub schedule_enabled: bool,
    pub schedule_start_hour: u8,
    pub schedule_start_minute: u8,
    pub schedule_stop_hour: u8,
    pub schedule_stop_minute: u8,
    pub hide_console: bool,
    pub tailscale: bool,
    /// Open Navidrome in the native GTK client instead of the WebKit-hosted
    /// window. The native client needs a fraction of the memory.
    pub native_client: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_music_dir: Option<PathBuf>,
    pub discord_rpc_enabled: bool,
    /// Preferred external tunnel provider for Discord artwork instructions.
    pub discord_artwork_tunnel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discord_rpc_bin: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discord_rpc_config: Option<PathBuf>,
    /// Compose file of an Invidious deployment that SyncPulse detected or that
    /// the user pointed at, instead of a deployment created by SyncPulse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invidious_compose_path: Option<PathBuf>,
    /// Navidrome account used to trigger an immediate library rescan through
    /// the Subsonic API. Optional; without it Navidrome still scans on schedule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navidrome_username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navidrome_password: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        let proj = project_dirs();
        let base = proj
            .map(|p| p.config_dir().to_path_buf())
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
                home.join(".config").join("navidrome-companion")
            });

        Self {
            setup_completed: false,
            navidrome_port: 4533,
            invidious_port: 3000,
            container_engine: ContainerEngine::default(),
            network_access: "local".to_owned(),
            music_dir: base.join("music"),
            data_dir: base.join("data"),
            bin_dir: base.join("bin"),
            plugins_dir: base.join("plugins"),
            auto_start: true,
            schedule_enabled: false,
            schedule_start_hour: 8,
            schedule_start_minute: 0,
            schedule_stop_hour: 23,
            schedule_stop_minute: 0,
            hide_console: true,
            tailscale: false,
            native_client: false,
            last_music_dir: None,
            discord_rpc_enabled: false,
            discord_artwork_tunnel: "none".to_owned(),
            discord_rpc_bin: None,
            discord_rpc_config: None,
            invidious_compose_path: None,
            navidrome_username: None,
            navidrome_password: None,
        }
    }
}

impl Config {
    pub fn normalize(&mut self) {
        self.navidrome_port = self.navidrome_port.max(1);
        self.invidious_port = self.invidious_port.max(1);
        self.schedule_start_hour = self.schedule_start_hour.min(23);
        self.schedule_start_minute = self.schedule_start_minute.min(59);
        self.schedule_stop_hour = self.schedule_stop_hour.min(23);
        self.schedule_stop_minute = self.schedule_stop_minute.min(59);
        if !matches!(
            self.discord_artwork_tunnel.as_str(),
            "none" | "tailscale" | "cloudflare"
        ) {
            self.discord_artwork_tunnel = "none".to_owned();
        }
        if !matches!(self.network_access.as_str(), "local" | "lan" | "tailscale") {
            self.network_access = "local".to_owned();
        }
        // Kept as a legacy serialized setting for older profiles; it no longer
        // controls artwork exposure or phone service access.
        self.tailscale = self.discord_artwork_tunnel == "tailscale";
    }
}

pub fn schedule_is_active(config: &Config, current_minutes: u16) -> bool {
    if !config.schedule_enabled {
        return false;
    }
    let start =
        u16::from(config.schedule_start_hour) * 60 + u16::from(config.schedule_start_minute);
    let stop = u16::from(config.schedule_stop_hour) * 60 + u16::from(config.schedule_stop_minute);
    if start == stop {
        return false;
    }
    if start < stop {
        (start..stop).contains(&current_minutes)
    } else {
        current_minutes >= start || current_minutes < stop
    }
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("org", "NavidromeHub", "navidrome-companion")
}

pub fn config_dir() -> PathBuf {
    let proj = project_dirs();
    if let Some(p) = proj {
        let dir = p.config_dir().to_path_buf();
        if let Err(error) = fs::create_dir_all(&dir) {
            tracing::warn!(
                "Unable to create config directory {}: {error}",
                dir.display()
            );
        }
        return dir;
    }

    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = home.join(".config").join("navidrome-companion");
    if let Err(error) = fs::create_dir_all(&dir) {
        tracing::warn!(
            "Unable to create config directory {}: {error}",
            dir.display()
        );
    }
    dir
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn ensure_dirs(cfg: &Config) -> Result<()> {
    fs::create_dir_all(&cfg.bin_dir).context("Failed to create bin_dir")?;
    fs::create_dir_all(&cfg.data_dir).context("Failed to create data_dir")?;
    fs::create_dir_all(&cfg.music_dir).context("Failed to create music_dir")?;
    fs::create_dir_all(&cfg.plugins_dir).context("Failed to create plugins_dir")?;
    Ok(())
}

pub fn load() -> Config {
    let path = config_path();
    if !path.exists() {
        let cfg = Config::default();
        if let Err(error) = save(&cfg) {
            tracing::warn!("Unable to write default configuration: {error:#}");
        }
        if let Err(error) = ensure_dirs(&cfg) {
            tracing::warn!("Unable to create application directories: {error:#}");
        }
        return cfg;
    }

    match fs::read_to_string(&path) {
        Ok(s) => match parse_config(&s) {
            Ok(mut cfg) => {
                let defaults = Config::default();
                if cfg.music_dir.as_os_str().is_empty() {
                    cfg.music_dir = defaults.music_dir;
                }
                if cfg.data_dir.as_os_str().is_empty() {
                    cfg.data_dir = defaults.data_dir;
                }
                if cfg.bin_dir.as_os_str().is_empty() {
                    cfg.bin_dir = defaults.bin_dir;
                }
                if cfg.plugins_dir.as_os_str().is_empty() {
                    cfg.plugins_dir = defaults.plugins_dir;
                }
                if let Err(error) = ensure_dirs(&cfg) {
                    tracing::warn!("Unable to create application directories: {error:#}");
                }
                cfg.normalize();
                cfg
            }
            Err(e) => {
                tracing::warn!("Failed to parse config.toml: {e}. Falling back to defaults.");
                let cfg = Config::default();
                if let Err(error) = ensure_dirs(&cfg) {
                    tracing::warn!("Unable to create application directories: {error:#}");
                }
                cfg
            }
        },
        Err(e) => {
            tracing::warn!("Failed to read config.toml: {e}. Falling back to defaults.");
            let cfg = Config::default();
            if let Err(error) = ensure_dirs(&cfg) {
                tracing::warn!("Unable to create application directories: {error:#}");
            }
            cfg
        }
    }
}

fn parse_config(contents: &str) -> Result<Config, toml::de::Error> {
    let mut cfg = toml::from_str::<Config>(contents)?;
    let has_setup_state = toml::from_str::<toml::Value>(contents)
        .ok()
        .and_then(|value| value.get("setup_completed").cloned())
        .is_some();
    if !has_setup_state {
        cfg.setup_completed = true;
    }
    Ok(cfg)
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path();
    let s = toml::to_string_pretty(cfg).context("Failed to serialize config")?;
    let parent = path
        .parent()
        .context("Configuration path has no parent directory")?;
    fs::create_dir_all(parent).context("Failed to create configuration directory")?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).context("Failed to create temporary config")?;
    use std::io::Write;
    temporary
        .write_all(s.as_bytes())
        .context("Failed to write temporary config")?;
    temporary
        .as_file()
        .sync_all()
        .context("Failed to flush temporary config")?;
    temporary
        .persist(&path)
        .context("Failed to atomically replace config.toml")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{schedule_is_active, Config, ContainerEngine};

    #[test]
    fn daily_schedule_handles_daytime_and_overnight_windows() {
        let mut config = Config {
            schedule_enabled: true,
            schedule_start_hour: 8,
            schedule_start_minute: 30,
            schedule_stop_hour: 17,
            schedule_stop_minute: 0,
            ..Config::default()
        };
        assert!(!schedule_is_active(&config, 8 * 60 + 29));
        assert!(schedule_is_active(&config, 8 * 60 + 30));
        assert!(!schedule_is_active(&config, 17 * 60));

        config.schedule_start_hour = 22;
        config.schedule_stop_hour = 6;
        assert!(schedule_is_active(&config, 23 * 60));
        assert!(schedule_is_active(&config, 5 * 60 + 59));
        assert!(!schedule_is_active(&config, 12 * 60));
    }

    #[test]
    fn equal_schedule_times_mean_disabled_window() {
        let config = Config {
            schedule_enabled: true,
            schedule_start_hour: 9,
            schedule_stop_hour: 9,
            ..Config::default()
        };
        assert!(!schedule_is_active(&config, 9 * 60));
        assert!(!schedule_is_active(&config, 10 * 60));
    }

    #[test]
    fn old_settings_files_get_safe_container_and_discord_defaults() {
        let config = super::parse_config("navidrome_port = 4533\n").unwrap();
        assert_eq!(config.container_engine, ContainerEngine::Docker);
        assert!(!config.discord_rpc_enabled);
        assert_eq!(config.discord_artwork_tunnel, "none");
        assert!(config.setup_completed);
    }

    #[test]
    fn setup_state_is_preserved_when_loading_a_new_profile() {
        let config = super::parse_config("setup_completed = false\n").unwrap();
        assert!(!config.setup_completed);
    }

    #[test]
    fn fresh_default_config_requires_initial_setup() {
        assert!(!Config::default().setup_completed);
    }

    #[test]
    fn old_settings_files_keep_the_browser_window() {
        let config = super::parse_config("navidrome_port = 4533\n").unwrap();
        assert!(!config.native_client);
    }

    #[test]
    fn the_native_client_preference_survives_a_save_and_load() {
        let config = super::parse_config("native_client = true\n").unwrap();
        assert!(config.native_client);
    }
}
