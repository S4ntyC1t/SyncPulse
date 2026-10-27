#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod children;
mod config;
mod desktop;
mod discovery;
mod downloader;
mod installer;
mod invidious;
#[cfg(target_os = "linux")]
mod native_client;
mod network;
mod plugins;
mod process;
mod service_window;
mod utils;
mod webkit_governor;
mod webview_compat;

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Result;
use chrono::{Local, Timelike};
use serde::Serialize;
use tauri::{image::Image, AppHandle, Emitter, Manager, State};
#[cfg(not(target_os = "linux"))]
use tauri::{WebviewUrl, WebviewWindowBuilder};
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use config::Config;
use discovery::Discoverer;
use downloader::{AudioFormat, AudioQuality, DownloadProgress, DownloadRequest, Downloader};
use installer::{InstallProgress, Installer};
use invidious::{InvidiousAppSettings, InvidiousManager, InvidiousProgress};
use plugins::{DiscordRpcConfig, PluginManager, PluginProgress};
use process::ProcessManager;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceWindow {
    Navidrome,
    Invidious,
}

impl ServiceWindow {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "navidrome" => Ok(Self::Navidrome),
            "invidious" => Ok(Self::Invidious),
            _ => Err(format!("Unknown service window: {value}")),
        }
    }

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Navidrome => "org.navidromehub.syncpulse.navidrome",
            Self::Invidious => "org.navidromehub.syncpulse.invidious",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Navidrome => "navidrome-service",
            Self::Invidious => "invidious-service",
        }
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Navidrome => "Navidrome",
            Self::Invidious => "Invidious",
        }
    }

    pub(crate) fn icon(self) -> &'static [u8] {
        match self {
            Self::Navidrome => include_bytes!("../icons/navidrome.png"),
            Self::Invidious => include_bytes!("../icons/invidious.png"),
        }
    }
}

struct RuntimeState {
    config: Config,
    processes: ProcessManager,
}

#[derive(Clone)]
struct SharedLogWriter(Arc<Mutex<File>>);

struct LockedLogWriter<'a>(MutexGuard<'a, File>);

impl Write for LockedLogWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> MakeWriter<'a> for SharedLogWriter {
    type Writer = LockedLogWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LockedLogWriter(
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}

struct AppState {
    runtime: Mutex<RuntimeState>,
    system: Mutex<sysinfo::System>,
    /// Cached `inspect_system_setup` result with the instant it was produced.
    /// Building it spawns `docker`/`podman`/`tailscale` and probes the network,
    /// which must not run on every dashboard poll.
    system_setup: Mutex<Option<(std::time::Instant, SystemSetupStatus)>>,
    discoverer: Discoverer,
    installer: Installer,
    downloader: Downloader,
    invidious: InvidiousManager,
    plugins: PluginManager,
}

impl AppState {
    fn new() -> Self {
        let config = config::load();
        if let Err(error) = config::ensure_dirs(&config) {
            tracing::error!("Unable to create application directories: {error:#}");
        }
        let installer = Installer::new();
        let plugins = PluginManager::new();
        let mut processes = ProcessManager::new();
        let navidrome_path = Installer::navidrome_bin_path(&config);
        let (navidrome, _) = processes.find_managed_processes(
            &navidrome_path,
            &PluginManager::discord_rpc_bin_path(&config),
        );
        let installed = Installer::is_navidrome_installed(&config);
        let should_start = if config.schedule_enabled {
            config::schedule_is_active(&config, local_minutes())
        } else {
            config.auto_start
        } && navidrome.is_none()
            && installed;
        if config.setup_completed && should_start {
            if let Err(error) = installer.start_navidrome(&config) {
                tracing::error!("Unable to auto-start Navidrome: {error:#}");
            }
        }
        Self {
            runtime: Mutex::new(RuntimeState { config, processes }),
            // Dashboard metrics do not need a second full process-table scan.
            system: Mutex::new(sysinfo::System::new_with_specifics(
                sysinfo::RefreshKind::nothing()
                    .with_cpu(sysinfo::CpuRefreshKind::everything())
                    .with_memory(sysinfo::MemoryRefreshKind::everything()),
            )),
            system_setup: Mutex::new(None),
            discoverer: Discoverer::new(),
            installer,
            downloader: Downloader::new(),
            invidious: InvidiousManager::new(),
            plugins,
        }
    }

    /// Returns the system setup status, reusing a recent result. The probe
    /// spawns external commands, so the dashboard's short poll interval must not
    /// rebuild it every time; the cache is dropped after `TTL`.
    fn system_setup(&self) -> SystemSetupStatus {
        const TTL: Duration = Duration::from_secs(45);
        if let Ok(mut cache) = self.system_setup.lock() {
            if let Some((created, status)) = cache.as_ref() {
                if created.elapsed() < TTL {
                    return status.clone();
                }
            }
            let status = inspect_system_setup();
            *cache = Some((std::time::Instant::now(), status.clone()));
            return status;
        }
        inspect_system_setup()
    }

    /// Forces the next `system_setup` call to re-probe (used after installing
    /// packages, when the wizard needs fresh information).
    fn invalidate_system_setup(&self) {
        if let Ok(mut cache) = self.system_setup.lock() {
            *cache = None;
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Dashboard {
    config: Config,
    first_run: bool,
    config_path: String,
    log_path: String,
    system_setup: SystemSetupStatus,
    navidrome_access_host: String,
    invidious_access_host: String,
    navidrome: NavidromeState,
    invidious_reachable: bool,
    invidious_installed: bool,
    /// Compose file SyncPulse can read and edit for the active Invidious
    /// deployment, whether managed here or detected as an external one.
    invidious_compose_path: Option<String>,
    discord_rpc_installed: bool,
    discord_rpc_running: bool,
    discord_rpc_pid: Option<u32>,
    discord_rpc_conflict: Option<String>,
    yt_dlp_installed: bool,
    ffmpeg_available: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SystemSetupStatus {
    supported: bool,
    distribution: String,
    package_manager: Option<String>,
    package_manager_available: bool,
    pkexec_available: bool,
    docker_installed: bool,
    compose_available: bool,
    podman_installed: bool,
    podman_compose_available: bool,
    tailscale_installed: bool,
    cloudflared_installed: bool,
    tailscale_connected: bool,
    suggested_engine: String,
    suggested_package_list: Vec<String>,
    warnings: Vec<String>,
    tailscale_ip: Option<String>,
    lan_ip: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DependencyInstallResult {
    installed_packages: Vec<String>,
    tailscale_enabled: bool,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SystemMetrics {
    cpu_percent: f32,
    used_memory_mb: u64,
    total_memory_mb: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NavidromeState {
    reachable: bool,
    managed_binary: bool,
    managed_process: bool,
    pid: Option<u32>,
    status: &'static str,
    detail: &'static str,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressPayload {
    percent: f32,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadOutcome {
    files: Vec<String>,
}

fn bridge_progress<T, F>(app: &AppHandle, event: &'static str, receiver: Receiver<T>, convert: F)
where
    T: Send + 'static,
    F: Fn(T) -> ProgressPayload + Send + 'static,
{
    let app = app.clone();
    std::thread::spawn(move || {
        for progress in receiver {
            if let Err(error) = app.emit(event, convert(progress)) {
                tracing::warn!("Unable to emit {event} progress: {error}");
            }
        }
    });
}

fn progress_payload(percent: f32, message: String) -> ProgressPayload {
    ProgressPayload { percent, message }
}

fn inspect_system_setup() -> SystemSetupStatus {
    #[cfg(target_os = "linux")]
    {
        use std::path::Path;

        let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let mut distribution = String::from("Linux (unknown distribution)");
        let mut id = String::new();
        let mut id_like = String::new();
        for line in os_release.lines() {
            if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                distribution = value.trim_matches('"').to_owned();
            } else if let Some(value) = line.strip_prefix("ID=") {
                id = value.trim_matches('"').to_ascii_lowercase();
            } else if let Some(value) = line.strip_prefix("ID_LIKE=") {
                id_like = value.trim_matches('"').to_ascii_lowercase();
            }
        }
        let arch_family = id == "arch"
            || id == "cachyos"
            || id_like.split_whitespace().any(|part| part == "arch");
        let package_manager = if arch_family && Path::new("/usr/bin/pacman").is_file() {
            Some("pacman")
        } else if (id == "debian" || id == "ubuntu" || id_like.contains("debian"))
            && Path::new("/usr/bin/apt-get").is_file()
        {
            Some("apt-get")
        } else if (id == "fedora" || id_like.contains("fedora"))
            && Path::new("/usr/bin/dnf").is_file()
        {
            Some("dnf")
        } else {
            None
        };
        let docker_installed = command_exists("docker");
        let compose_available =
            docker_installed && command_succeeds("docker", &["compose", "version"]);
        let podman_installed = command_exists("podman");
        let podman_compose_available =
            podman_installed && command_succeeds("podman", &["compose", "version"]);
        let tailscale_installed = command_exists("tailscale");
        let tailscale_ip = if tailscale_installed {
            crate::network::tailscale_ipv4()
                .ok()
                .map(|ip| ip.to_string())
        } else {
            None
        };
        let tailscale_connected = tailscale_ip.is_some();
        let lan_ip = crate::network::lan_ipv4().map(|ip| ip.to_string());
        let cloudflared_installed = command_exists("cloudflared");
        let suggested_engine = if podman_compose_available {
            "podman"
        } else if compose_available {
            "docker"
        } else if package_manager.is_some() {
            "podman"
        } else {
            "docker"
        };
        let suggested_package_list = match package_manager {
            Some("pacman") => vec!["podman".to_owned(), "podman-compose".to_owned()],
            Some("apt-get") => vec!["podman".to_owned(), "podman-compose".to_owned()],
            Some("dnf") => vec!["podman".to_owned(), "podman-compose".to_owned()],
            _ => Vec::new(),
        };
        let mut warnings = Vec::new();
        if !podman_installed {
            warnings.push("The first installation needs an internet connection to download packages and Invidious images.".to_owned());
        }
        if arch_family && !podman_compose_available {
            warnings.push("Podman Compose may require starting a rootless Podman user service after installation.".to_owned());
        }
        if !Path::new("/usr/bin/pkexec").is_file() {
            warnings.push("pkexec is not installed; SyncPulse cannot open a system authorization dialog for package installation.".to_owned());
        }
        SystemSetupStatus {
            supported: package_manager.is_some(),
            distribution,
            package_manager: package_manager.map(str::to_owned),
            package_manager_available: package_manager.is_some(),
            pkexec_available: Path::new("/usr/bin/pkexec").is_file(),
            docker_installed,
            compose_available,
            podman_installed,
            podman_compose_available,
            tailscale_installed,
            cloudflared_installed,
            tailscale_connected,
            suggested_engine: suggested_engine.to_owned(),
            suggested_package_list,
            warnings,
            tailscale_ip,
            lan_ip,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        SystemSetupStatus {
            supported: false,
            distribution: std::env::consts::OS.to_owned(),
            package_manager: None,
            package_manager_available: false,
            pkexec_available: false,
            docker_installed: command_exists("docker"),
            compose_available: command_succeeds("docker", &["compose", "version"]),
            podman_installed: command_exists("podman"),
            podman_compose_available: command_succeeds("podman", &["compose", "version"]),
            tailscale_installed: command_exists("tailscale"),
            cloudflared_installed: command_exists("cloudflared"),
            tailscale_connected: false,
            suggested_engine: "docker".to_owned(),
            suggested_package_list: Vec::new(),
            warnings: vec!["Guided system package installation is currently supported on CachyOS/Arch Linux, Debian/Ubuntu, and Fedora.".to_owned()],
            tailscale_ip: None,
            lan_ip: None,
        }
    }
}

#[tauri::command]
async fn install_system_dependencies(
    state: State<'_, AppState>,
    engine: String,
    include_tailscale: bool,
) -> Result<DependencyInstallResult, String> {
    let result = tokio::task::spawn_blocking(move || {
        install_system_dependencies_blocking(engine, include_tailscale)
    })
    .await
    .map_err(|error| format!("Dependency installer task failed: {error}"))??;
    // The probe result is stale now that packages or services changed.
    state.invalidate_system_setup();
    Ok(result)
}

fn install_system_dependencies_blocking(
    engine: String,
    include_tailscale: bool,
) -> Result<DependencyInstallResult, String> {
    if engine == "none" && !include_tailscale {
        return Err("No system packages or services were selected.".to_owned());
    }
    let status = inspect_system_setup();
    if !status.pkexec_available
        || (!status.supported && !(engine == "none" && status.tailscale_installed))
    {
        return Err("Guided package installation requires a supported Linux package manager and pkexec/Polkit. No system changes were made.".to_owned());
    }
    if !status.tailscale_installed && status.tailscale_connected {
        // Defensive invariant; a Tailscale IP cannot exist without the CLI.
        return Err(
            "Inconsistent Tailscale status detected; refresh SyncPulse and retry.".to_owned(),
        );
    }
    if !matches!(engine.as_str(), "podman" | "none") || (engine == "none" && !include_tailscale) {
        return Err("For a clean install, SyncPulse only installs rootless Podman automatically. Docker installation requires separate administrator setup because Docker group membership grants root-equivalent access.".to_owned());
    }
    let package_manager = status.package_manager.as_deref().unwrap_or_default();
    let mut packages = match (package_manager, engine.as_str()) {
        (_, "none") => Vec::new(),
        ("pacman", "podman") => vec!["podman", "podman-compose"],
        ("apt-get", "podman") => vec!["podman", "podman-compose"],
        ("dnf", "podman") => vec!["podman", "podman-compose"],
        _ => return Err("Unsupported distribution/package manager combination".to_owned()),
    };
    if include_tailscale && !status.tailscale_installed && package_manager == "pacman" {
        packages.push("tailscale");
    }
    if include_tailscale && !status.tailscale_installed && package_manager != "pacman" {
        return Err(
            "Automatic Tailscale installation is currently supported on CachyOS/Arch only."
                .to_owned(),
        );
    }
    if !packages.is_empty() {
        let (program, mut args): (&str, Vec<&str>) = match package_manager {
            "pacman" => ("/usr/bin/pacman", vec!["-S", "--needed", "--noconfirm"]),
            "apt-get" => ("/usr/bin/apt-get", vec!["install", "-y"]),
            "dnf" => ("/usr/bin/dnf", vec!["install", "-y"]),
            _ => return Err("Unsupported package manager".to_owned()),
        };
        args.extend(packages.iter().copied());
        if package_manager == "apt-get" {
            run_authorized_command("/usr/bin/apt-get", &["update"])?;
        }
        run_authorized_command(program, &args)?;
    }
    if include_tailscale && (status.tailscale_installed || packages.contains(&"tailscale")) {
        run_authorized_command(
            "/usr/bin/systemctl",
            &["enable", "--now", "tailscaled.service"],
        )?;
    }
    let message = format!(
        "Installed {} through the {} system package manager. No password was given to or stored by SyncPulse.",
        if packages.is_empty() { "no container packages".to_owned() } else { packages.join(", ") }, package_manager
    );
    Ok(DependencyInstallResult {
        installed_packages: packages.into_iter().map(str::to_owned).collect(),
        tailscale_enabled: include_tailscale,
        message,
    })
}

/// Only fixed system binaries and validated fixed arguments run with elevated
/// privileges; SyncPulse itself is never relaunched as root.
fn run_authorized_command(program: &str, args: &[&str]) -> Result<(), String> {
    if !system_operation_is_allowlisted(program, args) {
        return Err("System operation is not in SyncPulse's allowlist".to_owned());
    }
    let output = Command::new("/usr/bin/pkexec")
        .arg(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Unable to launch the system authorization dialog: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if detail.is_empty() {
        format!(
            "System operation was cancelled or failed ({})",
            output.status
        )
    } else {
        detail
    })
}

fn system_operation_is_allowlisted(program: &str, args: &[&str]) -> bool {
    match (program, args.first().copied()) {
        ("/usr/bin/pacman", Some("-S")) => {
            args.len() >= 4
                && args[1..3] == ["--needed", "--noconfirm"]
                && args[3..]
                    .iter()
                    .all(|p| matches!(*p, "podman" | "podman-compose" | "tailscale"))
        }
        ("/usr/bin/apt-get", Some("update")) => args == ["update"],
        ("/usr/bin/apt-get", Some("install")) => {
            args.len() >= 3
                && args[1] == "-y"
                && !args[2..].is_empty()
                && args[2..]
                    .iter()
                    .all(|p| matches!(*p, "podman" | "podman-compose"))
        }
        ("/usr/bin/dnf", Some("install")) => {
            args.len() >= 3
                && args[1] == "-y"
                && !args[2..].is_empty()
                && args[2..]
                    .iter()
                    .all(|p| matches!(*p, "podman" | "podman-compose"))
        }
        ("/usr/bin/systemctl", Some("enable")) => args == ["enable", "--now", "tailscaled.service"],
        _ => false,
    }
}

fn command_exists(program: &str) -> bool {
    Command::new("sh")
        .args([
            "-c",
            "command -v \"$1\" >/dev/null 2>&1",
            "syncpulse",
            program,
        ])
        .status()
        .is_ok_and(|status| status.success())
}

fn command_succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[tauri::command]
async fn dashboard(state: State<'_, AppState>) -> Result<Dashboard, String> {
    let (config, navidrome_process, rpc_process) = {
        let mut runtime = state
            .runtime
            .lock()
            .map_err(|_| "Application state lock was poisoned".to_owned())?;
        let config = runtime.config.clone();
        let (navidrome, rpc) = runtime.processes.find_managed_processes(
            &Installer::navidrome_bin_path(&config),
            &PluginManager::discord_rpc_bin_path(&config),
        );
        (config, navidrome, rpc)
    };
    let config = remember_external_invidious(state.inner(), config).await;
    let navidrome_managed = Installer::is_navidrome_installed(&config);
    let invidious_managed = InvidiousManager::is_installed(&config);
    let navidrome_probe_host = network::service_probe_address_for(&config, navidrome_managed).ok();
    let invidious_probe_host = if invidious_managed {
        network::service_probe_address_for_configured_mode(&config)
    } else {
        "127.0.0.1".to_owned()
    };
    let (navidrome_ok, invidious_ok) = tokio::join!(
        async {
            match navidrome_probe_host.as_deref() {
                Some(host) => {
                    state
                        .discoverer
                        .check_navidrome_at(host, config.navidrome_port)
                        .await
                }
                None if !navidrome_managed => {
                    state
                        .discoverer
                        .check_navidrome(config.navidrome_port)
                        .await
                }
                None => false,
            }
        },
        async {
            state
                .discoverer
                .check_invidious_at(&invidious_probe_host, config.invidious_port)
                .await
        }
    );
    let managed_binary = Installer::is_navidrome_installed(&config);
    let managed_process = navidrome_process.is_some();
    let (status, detail) = navidrome_status(navidrome_ok, managed_process, managed_binary);
    let invidious_managed = InvidiousManager::is_installed(&config);
    let invidious_compose_path = InvidiousManager::has_editable_compose(&config).then(|| {
        InvidiousManager::active_compose_path(&config)
            .display()
            .to_string()
    });
    let system_setup = state.inner().system_setup();
    Ok(Dashboard {
        first_run: !config.setup_completed,
        navidrome: NavidromeState {
            reachable: navidrome_ok,
            managed_binary,
            managed_process,
            pid: navidrome_process.map(|process| process.pid),
            status,
            detail,
        },
        invidious_reachable: invidious_ok,
        invidious_installed: InvidiousManager::is_installed(&config),
        invidious_compose_path,
        discord_rpc_installed: PluginManager::is_discord_rpc_installed(&config),
        discord_rpc_running: rpc_process.is_some(),
        discord_rpc_pid: rpc_process.map(|process| process.pid),
        discord_rpc_conflict: PluginManager::discord_rpc_conflict(&config),
        yt_dlp_installed: Downloader::is_yt_dlp_installed(&config),
        ffmpeg_available: Downloader::is_ffmpeg_available(&config),
        config_path: config::config_path().display().to_string(),
        log_path: config::config_dir()
            .join("syncpulse.log")
            .display()
            .to_string(),
        system_setup,
        navidrome_access_host: network::service_advertised_host(&config, navidrome_managed)
            .unwrap_or_default(),
        invidious_access_host: network::service_advertised_host(&config, invidious_managed)
            .unwrap_or_default(),
        config,
    })
}

/// Detects an Invidious deployment SyncPulse did not create, records its
/// Compose file once (so it stays editable here), and returns the configuration.
async fn remember_external_invidious(state: &AppState, mut config: Config) -> Config {
    if InvidiousManager::is_installed(&config)
        || config
            .invidious_compose_path
            .as_ref()
            .is_some_and(|path| path.is_file())
    {
        return config;
    }
    let Some(path) = InvidiousManager::detect_external_compose(&config).await else {
        return config;
    };
    config.invidious_compose_path = Some(path);
    if let Ok(mut runtime) = state.runtime.lock() {
        runtime.config = config.clone();
    }
    if let Err(error) = config::save(&config) {
        tracing::warn!("Unable to remember the detected Invidious Compose file: {error:#}");
    }
    config
}

fn navidrome_status(
    reachable: bool,
    managed_process: bool,
    managed_binary: bool,
) -> (&'static str, &'static str) {
    match (reachable, managed_process, managed_binary) {
        (true, true, _) => ("RUNNING", "Managed by SyncPulse and responding."),
        (true, false, _) => (
            "RESPONDING",
            "A Navidrome service is responding on this port. It is running outside SyncPulse.",
        ),
        (false, true, _) => (
            "PROCESS ONLY",
            "The SyncPulse-managed process exists, but its web interface is not responding yet.",
        ),
        (false, false, true) => (
            "STOPPED",
            "The managed binary is installed, but the service is not responding.",
        ),
        (false, false, false) => (
            "NOT DETECTED",
            "No service responded on this port. No SyncPulse-managed binary was found.",
        ),
    }
}

#[tauri::command]
fn system_metrics(state: State<'_, AppState>) -> Result<SystemMetrics, String> {
    let mut system = state
        .system
        .lock()
        .map_err(|_| "System metrics lock was poisoned".to_owned())?;
    system.refresh_memory();
    system.refresh_cpu_usage();
    Ok(SystemMetrics {
        cpu_percent: system.global_cpu_usage(),
        used_memory_mb: system.used_memory() / (1024 * 1024),
        total_memory_mb: system.total_memory() / (1024 * 1024),
    })
}

#[tauri::command]
fn select_directory(title: String) -> Result<Option<String>, String> {
    #[cfg(target_os = "linux")]
    {
        for (program, args) in [
            (
                "kdialog",
                vec!["--getexistingdirectory", ".", title.as_str()],
            ),
            (
                "zenity",
                vec!["--file-selection", "--directory", "--title", &title],
            ),
        ] {
            if Command::new(program).arg("--version").output().is_err() {
                continue;
            }
            let output = Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .map_err(|error| format!("Unable to open the folder picker: {error}"))?;
            if !output.status.success() {
                return Ok(None);
            }
            let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            return Ok((!path.is_empty()).then_some(path));
        }
        Err(
            "Install kdialog or zenity to browse for folders, or enter the path directly."
                .to_owned(),
        )
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = title;
        Err(
            "Folder browsing is not available on this platform yet; enter the path directly."
                .to_owned(),
        )
    }
}

#[tauri::command]
fn complete_initial_setup(state: State<'_, AppState>) -> Result<(), String> {
    let mut runtime = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?;
    let mut config = runtime.config.clone();
    config.setup_completed = true;
    config::save(&config).map_err(|error| format!("Unable to save setup completion: {error:#}"))?;
    runtime.config = config;
    Ok(())
}

#[tauri::command]
async fn install_navidrome(app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    let config = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?
        .config
        .clone();
    let (tx, rx) = mpsc::channel::<InstallProgress>();
    bridge_progress(&app, "operation-progress", rx, |progress| {
        progress_payload(progress.percent, progress.message)
    });
    let path = state
        .installer
        .install_navidrome(&config, tx.clone())
        .await
        .map_err(|error| format!("Navidrome installation failed: {error:#}"))?;
    let probe_host = network::service_probe_address(&config)
        .map_err(|error| format!("Unable to determine Navidrome network address: {error:#}"))?;
    if !state
        .discoverer
        .check_navidrome_at(&probe_host, config.navidrome_port)
        .await
    {
        let _ = tx.send(InstallProgress {
            percent: 94.0,
            message: "Starting Navidrome and verifying its health endpoint".to_owned(),
        });
        state.installer.start_navidrome(&config).map_err(|error| {
            format!(
                "Navidrome binary verified at {}, but the service could not be started: {error:#}",
                path.display()
            )
        })?;
        let started = tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                if state
                    .discoverer
                    .check_navidrome_at(&probe_host, config.navidrome_port)
                    .await
                {
                    return true;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        })
        .await
        .unwrap_or(false);
        if !started {
            return Err(format!(
                "Navidrome binary verified at {}, but /ping did not respond on port {} within 90 seconds. Check the service logs and configured data/music folders.",
                path.display(),
                config.navidrome_port
            ));
        }
    }
    let _ = tx.send(InstallProgress {
        percent: 100.0,
        message: "Navidrome installation and service health verified".to_owned(),
    });
    Ok(format!(
        "Navidrome installed and verified at {}",
        path.display()
    ))
}

#[tauri::command]
fn start_navidrome(state: State<'_, AppState>) -> Result<(), String> {
    let runtime = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?;
    state
        .installer
        .start_navidrome(&runtime.config)
        .map_err(|error| format!("Unable to start Navidrome: {error:#}"))
}

#[tauri::command]
fn stop_navidrome(state: State<'_, AppState>) -> Result<(), String> {
    let mut runtime = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?;
    let binary = Installer::navidrome_bin_path(&runtime.config);
    runtime
        .processes
        .stop_navidrome_at(&binary)
        .map(|_| ())
        .map_err(|error| format!("Unable to stop Navidrome: {error:#}"))
}

#[tauri::command]
async fn download_audio(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
    format: String,
    quality: String,
) -> Result<DownloadOutcome, String> {
    let config = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?
        .config
        .clone();
    let format = match format.as_str() {
        "mp3" => AudioFormat::Mp3,
        "flac" => AudioFormat::Flac,
        "m4a" => AudioFormat::M4a,
        _ => return Err("Unsupported audio format".into()),
    };
    let quality = match quality.as_str() {
        "best" => AudioQuality::Best,
        "320" => AudioQuality::Q320,
        _ => return Err("Unsupported audio quality".into()),
    };
    let (tx, rx) = mpsc::channel::<DownloadProgress>();
    bridge_progress(&app, "operation-progress", rx, |progress| {
        progress_payload(progress.percent, progress.message)
    });
    state
        .downloader
        .download_to_navidrome(
            &config,
            &DownloadRequest {
                url,
                format,
                quality,
            },
            &tx,
        )
        .await
        .map(|files| DownloadOutcome {
            files: files
                .into_iter()
                .map(|path| path.display().to_string())
                .collect(),
        })
        .map_err(|error| format!("Download failed: {error:#}"))
}

#[tauri::command]
async fn rescan_navidrome(state: State<'_, AppState>) -> Result<String, String> {
    let config = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?
        .config
        .clone();
    state
        .downloader
        .request_library_scan(&config)
        .await
        .map(|message| message.to_owned())
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn install_discord_rpc(app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    let mut config = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?
        .config
        .clone();
    let (tx, rx) = mpsc::channel::<PluginProgress>();
    bridge_progress(&app, "operation-progress", rx, |progress| {
        progress_payload(progress.percent, progress.message)
    });
    let path = state
        .plugins
        .install_discord_rpc(&mut config, Some(&tx))
        .await
        .map_err(|error| format!("Plugin installation failed: {error:#}"))?;
    let mut runtime = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?;
    runtime.config = config;
    Ok(format!(
        "Installed at {}. Configure the Navidrome account in Settings before enabling it.",
        path.display()
    ))
}

#[tauri::command]
async fn operate_invidious(
    app: AppHandle,
    state: State<'_, AppState>,
    action: String,
) -> Result<(), String> {
    let config = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?
        .config
        .clone();
    match action.as_str() {
        "install" => {
            let (tx, rx) = mpsc::channel::<InvidiousProgress>();
            bridge_progress(&app, "operation-progress", rx, |progress| {
                progress_payload(progress.percent, progress.message)
            });
            state
                .invidious
                .install(&config, config.invidious_port, &tx)
                .await
        }
        "start" => state.invidious.start(&config, config.invidious_port).await,
        "stop" => state.invidious.stop(&config).await,
        _ => return Err("Unsupported Invidious action".into()),
    }
    .map_err(|error| format!("Invidious operation failed: {error:#}"))
}

#[tauri::command]
fn save_settings(state: State<'_, AppState>, mut config: Config) -> Result<String, String> {
    config.normalize();
    let mut runtime = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?;
    if config.discord_rpc_enabled && PluginManager::is_discord_rpc_installed(&config) {
        let rpc_config = PluginManager::load_discord_rpc_config(&config)
            .map_err(|error| format!("Unable to read Discord RPC settings: {error:#}"))?;
        PluginManager::validate_discord_rpc_config(&rpc_config, true)
            .map_err(|error| format!("Discord RPC is enabled but not configured: {error:#}"))?;
    }
    let stop_managed_rpc = !config.discord_rpc_enabled && runtime.config.discord_rpc_enabled;
    let previous_config = runtime.config.clone();
    let music_dir_changed = config.music_dir != runtime.config.music_dir;
    let network_access_changed = config.network_access != runtime.config.network_access;
    let navidrome_port_changed = config.navidrome_port != runtime.config.navidrome_port;
    let invidious_port_changed = config.invidious_port != runtime.config.invidious_port;
    if invidious_port_changed
        && InvidiousManager::has_editable_compose(&config)
        && !InvidiousManager::is_installed(&config)
    {
        return Err("The detected Invidious deployment is user-managed. SyncPulse will not change its port mapping; edit its Compose file and save the matching port there instead.".to_owned());
    }
    if (network_access_changed || navidrome_port_changed || invidious_port_changed)
        && config.network_access == "tailscale"
    {
        network::tailscale_ipv4().map_err(|error| format!(
            "Cannot enable Tailscale-only service access yet: {error:#}. Sign in and connect Tailscale first; no settings were changed."
        ))?;
    }
    if (network_access_changed || navidrome_port_changed)
        && Installer::is_navidrome_installed(&config)
    {
        let (running, _) = runtime.processes.find_managed_processes(
            &Installer::navidrome_bin_path(&config),
            &PluginManager::discord_rpc_bin_path(&config),
        );
        if running.is_some() {
            return Err("Navidrome is already running. Stop it explicitly from Services (or your existing service manager), then save the network-access change; SyncPulse will not stop an existing process automatically.".to_owned());
        }
    }
    config::ensure_dirs(&config).map_err(|error| format!("Unable to create folders: {error:#}"))?;
    let mut invidious_binding_applied = false;
    let mut navidrome_binding_applied = false;
    if (network_access_changed || invidious_port_changed) && InvidiousManager::is_installed(&config)
    {
        let previous_probe = network::service_probe_address(&runtime.config)
            .unwrap_or_else(|_| "127.0.0.1".to_owned());
        let previous_endpoint = std::net::SocketAddr::new(
            previous_probe
                .parse::<std::net::IpAddr>()
                .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            runtime.config.invidious_port,
        );
        let accepting_connections =
            std::net::TcpStream::connect_timeout(&previous_endpoint, Duration::from_millis(150))
                .is_ok();
        if accepting_connections
            || tauri::async_runtime::block_on(
                state
                    .discoverer
                    .check_invidious_at(&previous_probe, runtime.config.invidious_port),
            )
        {
            return Err("Invidious is running. Stop it explicitly from Services, then save the network-access change; SyncPulse will not stop it automatically.".to_owned());
        }
        InvidiousManager::configure_managed_network_access(&config)
            .map_err(|error| format!("Unable to update Invidious network binding: {error:#}"))?;
        invidious_binding_applied = true;
        tracing::info!(
            "Invidious network binding changed to {}; restart the service to apply it",
            config.network_access
        );
    }
    if (network_access_changed || navidrome_port_changed)
        && Installer::is_navidrome_installed(&config)
    {
        installer::write_config(&config)
            .map_err(|error| format!("Unable to update Navidrome network binding: {error:#}"))?;
        navidrome_binding_applied = true;
        tracing::info!(
            "Navidrome network binding changed to {}; restart the service to apply it",
            config.network_access
        );
    }
    if music_dir_changed && Installer::is_navidrome_installed(&config) {
        installer::write_config(&config)
            .map_err(|error| format!("Unable to update Navidrome configuration: {error:#}"))?;
        tracing::info!(
            "Updated managed Navidrome MusicFolder to {} after a settings change; restart Navidrome to apply it",
            config.music_dir.display()
        );
    }
    config::save(&config).map_err(|error| format!("Unable to save settings: {error:#}"))?;
    runtime.config = config;
    if stop_managed_rpc {
        state
            .plugins
            .stop_managed_discord_rpc(&previous_config)
            .map_err(|error| {
                format!("Settings were saved, but the managed Discord Rich Presence worker did not stop: {error:#}")
            })?;
    } else if runtime.config.discord_rpc_enabled && !previous_config.discord_rpc_enabled {
        state
            .plugins
            .start_discord_rpc_if_enabled(&runtime.config)
            .map_err(|error| {
                format!("Settings were saved, but Discord RPC supervisor did not start: {error:#}")
            })?;
    }
    Ok(
        if music_dir_changed && Installer::is_navidrome_installed(&runtime.config) {
            "Settings saved. Navidrome's MusicFolder configuration was updated; restart Navidrome for the new folder to take effect.".to_owned()
        } else if network_access_changed {
            if invidious_binding_applied || navidrome_binding_applied {
                "Settings saved. Managed service bindings were updated. Stop and restart each service explicitly to apply the new address; SyncPulse did not stop running services. Public Funnel remains unchanged.".to_owned()
            } else {
                "Settings saved. No SyncPulse-managed services were present to rebind. Public Funnel remains unchanged.".to_owned()
            }
        } else {
            "Settings saved".to_owned()
        },
    )
}

#[tauri::command]
fn get_discord_rpc_config(state: State<'_, AppState>) -> Result<DiscordRpcConfig, String> {
    let config = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?
        .config
        .clone();
    PluginManager::load_discord_rpc_config(&config)
        .map_err(|error| format!("Unable to load Discord RPC settings: {error:#}"))
}

#[tauri::command]
fn save_discord_rpc_settings(
    state: State<'_, AppState>,
    rpc_config: DiscordRpcConfig,
) -> Result<(), String> {
    let config = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?
        .config
        .clone();
    PluginManager::save_discord_rpc_config(&config, &rpc_config)
        .map_err(|error| format!("Unable to save Discord RPC settings: {error:#}"))
}

#[tauri::command]
fn restart_discord_rpc(state: State<'_, AppState>) -> Result<(), String> {
    let runtime = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?;
    if !runtime.config.discord_rpc_enabled {
        return Err("Enable Discord Rich Presence in Settings before starting it".into());
    }
    let configured_binary = PluginManager::discord_rpc_bin_path(&runtime.config);
    if !configured_binary.is_file() {
        return Err("Install Discord Rich Presence before starting it".into());
    }
    state
        .plugins
        .restart_managed_discord_rpc(&runtime.config)
        .map_err(|error| format!("Unable to start Discord RPC: {error:#}"))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FunnelSetupResult {
    public_url: String,
    listener: String,
    https_port: u16,
}

#[tauri::command]
fn enable_artwork_funnel(
    state: State<'_, AppState>,
    https_port: u16,
    covers_listen: String,
) -> Result<FunnelSetupResult, String> {
    if !matches!(https_port, 443 | 8443 | 10000) {
        return Err("Tailscale Funnel HTTPS port must be 443, 8443, or 10000".into());
    }
    let listener: std::net::SocketAddr = covers_listen
        .parse()
        .map_err(|_| "Cover server address must look like 127.0.0.1:4534".to_owned())?;
    if !listener.ip().is_loopback() {
        return Err("For safety, Funnel can only publish the local loopback cover server".into());
    }
    let config = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?
        .config
        .clone();
    if !command_exists("tailscale") || network::tailscale_ipv4().is_err() {
        return Err("Tailscale must be installed, signed in, and connected before enabling public artwork Funnel. This operation publishes the artwork listener on the public internet, not just to your private tailnet.".to_owned());
    }
    state
        .plugins
        .start_artwork_server(&config, &listener.to_string())
        .map_err(|error| format!("Unable to start the private artwork server: {error:#}"))?;
    wait_for_loopback_listener(listener)?;

    #[cfg(target_os = "linux")]
    {
        let https_flag = format!("--https={https_port}");
        let listener_url = format!("http://{listener}");
        let mut command = Command::new("tailscale");
        command
            .args([
                "funnel",
                "--bg",
                "--yes",
                https_flag.as_str(),
                listener_url.as_str(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = command.output().map_err(|error| {
            format!("Unable to run Tailscale CLI (`tailscale`): {error}. Install Tailscale and sign in first.")
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let details = if !stderr.is_empty() { stderr } else { stdout };
            return Err(if details.is_empty() {
                format!("Tailscale Funnel failed with {}", output.status)
            } else {
                format!("Tailscale Funnel failed: {details}")
            });
        }

        let output_text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let public_url = extract_funnel_url(&output_text, https_port)
            .or_else(|| tailscale_public_url(https_port))
            .ok_or_else(|| {
                "Funnel was enabled, but SyncPulse could not determine its public hostname. Run `tailscale funnel status` and enter the HTTPS address shown there.".to_owned()
            })?;
        Ok(FunnelSetupResult {
            public_url,
            listener: listener.to_string(),
            https_port,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (listener, https_port);
        Err("One-click Tailscale Funnel setup is currently supported on Linux only".into())
    }
}

fn wait_for_loopback_listener(listener: std::net::SocketAddr) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(&listener, Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "The artwork server did not start listening on {listener} within 5 seconds"
    ))
}

#[cfg(target_os = "linux")]
fn extract_funnel_url(output: &str, https_port: u16) -> Option<String> {
    let expected_suffix = format!(".ts.net:{https_port}");
    output
        .split_whitespace()
        .map(|token| {
            token.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && !matches!(character, '.' | '-' | ':' | '/')
            })
        })
        .find(|token| {
            token.starts_with("https://")
                && token.contains(".ts.net")
                && (token.ends_with(&expected_suffix)
                    || (https_port == 443 && token.ends_with(".ts.net")))
        })
        .map(|token| token.trim_end_matches('/').to_owned())
}

#[cfg(target_os = "linux")]
fn tailscale_public_url(https_port: u16) -> Option<String> {
    let output = Command::new("tailscale")
        .args(["funnel", "status"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    extract_funnel_url(&text, https_port)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InvidiousComposeConfig {
    path: String,
    contents: String,
    app_settings: InvidiousAppSettings,
    app_config_supported: bool,
}

#[tauri::command]
fn get_invidious_compose(state: State<'_, AppState>) -> Result<InvidiousComposeConfig, String> {
    let config = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?
        .config
        .clone();
    let (app_settings, app_config_supported) = InvidiousManager::compose_app_settings(&config)
        .map_err(|error| format!("Unable to read Invidious app settings: {error:#}"))?;
    Ok(InvidiousComposeConfig {
        path: InvidiousManager::active_compose_path(&config)
            .display()
            .to_string(),
        contents: InvidiousManager::read_compose(&config)
            .map_err(|error| format!("Unable to load Invidious Compose file: {error:#}"))?,
        app_settings,
        app_config_supported,
    })
}

#[tauri::command]
fn save_invidious_compose(
    state: State<'_, AppState>,
    contents: String,
    app_settings: InvidiousAppSettings,
) -> Result<String, String> {
    let config = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?
        .config
        .clone();
    InvidiousManager::save_compose(&config, &contents, &app_settings)
        .map_err(|error| format!("Unable to save Invidious Compose file: {error:#}"))
}

#[tauri::command]
async fn validate_invidious_compose(state: State<'_, AppState>) -> Result<(), String> {
    let config = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?
        .config
        .clone();
    state
        .invidious
        .validate_compose(&config)
        .await
        .map_err(|error| format!("Invidious Compose validation failed: {error:#}"))
}

#[tauri::command]
async fn open_navidrome(state: State<'_, AppState>) -> Result<String, String> {
    let (port, host) = configured_service_endpoint(&state, true)?;
    ensure_navidrome_reachable(&state, &host, port).await?;
    // The user can ask for the native client instead of the WebKit window,
    // which needs a fraction of the memory.
    #[cfg(target_os = "linux")]
    if native_client_preferred(&state) {
        return spawn_native_client();
    }
    spawn_service_window(ServiceWindow::Navidrome, &host, port)
}

/// Whether Navidrome should open in the native client rather than in the
/// WebKit-hosted service window.
#[cfg(target_os = "linux")]
fn native_client_preferred(state: &State<'_, AppState>) -> bool {
    state
        .runtime
        .lock()
        .map(|runtime| runtime.config.native_client)
        .unwrap_or(false)
}

#[tauri::command]
async fn open_invidious(state: State<'_, AppState>) -> Result<String, String> {
    let (port, host) = configured_service_endpoint(&state, false)?;
    ensure_invidious_reachable(&state, &host, port).await?;
    spawn_service_window(ServiceWindow::Invidious, &host, port)
}

/// Opens the native Navidrome client: plain GTK widgets driven by the
/// Subsonic API, without a browser engine.
#[tauri::command]
async fn open_navidrome_client() -> Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        spawn_native_client()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("The native client is only available on Linux".to_owned())
    }
}

#[cfg(target_os = "linux")]
fn spawn_native_client() -> Result<String, String> {
    // One client at a time: it is a full music player, and a second copy
    // would fight over the audio device.
    if children::tag_running("navidrome-client") {
        return Ok("The native client is already open".to_owned());
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("Unable to locate the SyncPulse executable: {error}"))?;
    Command::new(executable)
        .arg("--syncpulse-client")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map(|child| children::track_tagged("navidrome-client", child))
        .map_err(|error| format!("Unable to start the native client: {error}"))?;
    Ok("Opening the native client".to_owned())
}

#[tauri::command]
async fn open_navidrome_browser(state: State<'_, AppState>) -> Result<(), String> {
    let (port, host) = configured_service_endpoint(&state, true)?;
    ensure_navidrome_reachable(&state, &host, port).await?;
    browser::open(&format!("http://{host}:{port}/"))
}

#[tauri::command]
async fn open_invidious_browser(state: State<'_, AppState>) -> Result<(), String> {
    let (port, host) = configured_service_endpoint(&state, false)?;
    ensure_invidious_reachable(&state, &host, port).await?;
    browser::open(&format!("http://{host}:{port}/"))
}

fn configured_service_endpoint(state: &AppState, navidrome: bool) -> Result<(u16, String), String> {
    let runtime = state
        .runtime
        .lock()
        .map_err(|_| "Application state lock was poisoned".to_owned())?;
    let port = if navidrome {
        runtime.config.navidrome_port
    } else {
        runtime.config.invidious_port
    };
    let managed = if navidrome {
        Installer::is_navidrome_installed(&runtime.config)
    } else {
        InvidiousManager::is_installed(&runtime.config)
    };
    let host = if !managed && !navidrome {
        "127.0.0.1".to_owned()
    } else {
        network::service_access_host_for(&runtime.config, managed)
            .map_err(|error| format!("Unable to determine the service address: {error:#}"))?
    };
    Ok((port, host))
}

async fn ensure_navidrome_reachable(state: &AppState, host: &str, port: u16) -> Result<(), String> {
    if state.discoverer.check_navidrome_at(host, port).await {
        Ok(())
    } else {
        Err(format!(
            "Navidrome is not responding on port {port}. Start or install the service first."
        ))
    }
}

async fn ensure_invidious_reachable(state: &AppState, host: &str, port: u16) -> Result<(), String> {
    if state.discoverer.check_invidious_at(host, port).await {
        Ok(())
    } else {
        Err(format!(
            "Invidious is not responding on its stats API at port {port}. Start or install the service first."
        ))
    }
}

fn spawn_service_window(service: ServiceWindow, host: &str, port: u16) -> Result<String, String> {
    // Each service opens in SyncPulse's own native system WebView (WebKitGTK on
    // Linux, WebView2 on Windows). The window runs as a separate standalone
    // process with its own application ID; no Chromium/Electron is involved.
    //
    // Only one window per service is kept: a running standalone WebView holds
    // its own WebKit processes (hundreds of megabytes), so opening a second one
    // for the same service is refused instead of silently piling up memory.
    if children::tag_running(service.label()) {
        tracing::info!(
            "{} window is already open; focusing the existing window is not possible, skipping a new one",
            service.title()
        );
        return Ok(format!("{} window is already open", service.title()));
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("Unable to locate the SyncPulse executable: {error}"))?;
    if !executable.is_file() {
        return Err(format!(
            "The running SyncPulse executable is no longer available at {}. Close and reopen SyncPulse, then open {} again.",
            executable.display(),
            service.title()
        ));
    }
    let label = service.label();
    Command::new(executable)
        .args([
            "--syncpulse-service",
            service.title().to_lowercase().as_str(),
        ])
        .arg(host)
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map(|child| children::track_tagged(label, child))
        .map_err(|error| format!("Unable to start the {} window: {error}", service.title()))?;
    Ok(format!("Opening the {} window", service.title()))
}

fn service_window_args() -> Result<Option<(ServiceWindow, String, u16)>, String> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--syncpulse-service") {
        return Ok(None);
    }
    let service = args
        .next()
        .ok_or_else(|| "Missing service name for standalone service window".to_owned())
        .and_then(|name| ServiceWindow::parse(&name))?;
    let host = args
        .next()
        .ok_or_else(|| "Missing service host for standalone service window".to_owned())?;
    if host != "configured" && host.parse::<std::net::IpAddr>().is_err() {
        return Err("Standalone service window host must be an IP address".to_owned());
    }
    let port = args
        .next()
        .ok_or_else(|| "Missing service port for standalone service window".to_owned())?
        .parse::<u16>()
        .map_err(|error| format!("Invalid standalone service port: {error}"))?;
    if port == 0 || args.next().is_some() {
        return Err("Standalone service window arguments are invalid".to_owned());
    }
    Ok(Some((service, host, port)))
}

fn run_service_window(service: ServiceWindow, host: String, port: u16) -> Result<(), String> {
    let host = if host == "configured" {
        let config = config::load();
        let managed = match service {
            ServiceWindow::Navidrome => Installer::is_navidrome_installed(&config),
            ServiceWindow::Invidious => InvidiousManager::is_installed(&config),
        };
        if matches!(service, ServiceWindow::Invidious) && !managed {
            "127.0.0.1".to_owned()
        } else {
            network::service_access_host_for(&config, managed).map_err(|error| {
                format!("Unable to determine {} address: {error:#}", service.title())
            })?
        }
    } else {
        host
    };
    let url = url::Url::parse(&format!("http://{host}:{port}/"))
        .map_err(|error| format!("Invalid local address for {}: {error}", service.title()))?;

    // On Linux the service shell is hosted directly on GTK + WebKitGTK so
    // SyncPulse can govern the WebKit context (memory budget, cache model,
    // feature set) and skip the Tauri/tao/wry runtime entirely.
    #[cfg(target_os = "linux")]
    {
        service_window::run(service, url)
    }

    #[cfg(not(target_os = "linux"))]
    {
        run_tauri_service_window(service, url)
    }
}

/// Non-Linux fallback: let Tauri host the service window (WebView2 on Windows).
#[cfg(not(target_os = "linux"))]
fn run_tauri_service_window(service: ServiceWindow, url: url::Url) -> Result<(), String> {
    desktop::set_linux_application_id(service.id());
    webview_compat::apply_linux_webview_env();
    let icon = Image::from_bytes(service.icon())
        .map_err(|error| format!("Unable to load the {} icon: {error}", service.title()))?;
    let mut context = tauri::generate_context!();
    let config = context.config_mut();
    config.identifier = service.id().to_owned();
    config.app.enable_gtk_app_id = false;
    for window in &mut config.app.windows {
        window.create = false;
    }
    tracing::info!(
        "Starting standalone {} window with desktop application ID {}",
        service.title(),
        service.id()
    );
    context.set_default_window_icon(Some(icon.clone()));
    let initialization_script = {
        let mut scripts = match service {
            ServiceWindow::Navidrome => webview_compat::navidrome_init_scripts(),
            ServiceWindow::Invidious => webview_compat::invidious_init_scripts(),
        };
        if webview_compat::console_logging_enabled() {
            scripts.push(webview_compat::diagnostic_script());
        }
        scripts.join("\n")
    };
    tauri::Builder::default()
        .setup(move |app| {
            WebviewWindowBuilder::new(app, service.label(), WebviewUrl::External(url))
                .title(service.title())
                .inner_size(1100.0, 760.0)
                .min_inner_size(720.0, 520.0)
                .decorations(true)
                .skip_taskbar(false)
                .visible(true)
                .zoom_hotkeys_enabled(false)
                .initialization_script(initialization_script)
                .icon(icon.clone())
                .map_err(std::io::Error::other)?
                .build()
                .map_err(std::io::Error::other)?;
            Ok(())
        })
        .run(context)
        .map_err(|error| format!("Unable to run the {} window: {error}", service.title()))
}

fn local_minutes() -> u16 {
    let now = Local::now().time();
    (now.hour() * 60 + now.minute()) as u16
}

fn start_schedule_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let state = app.state::<AppState>();
            let Ok(mut runtime) = state.runtime.lock() else {
                tracing::error!("Application state lock was poisoned in scheduler");
                continue;
            };
            if !runtime.config.setup_completed
                || !runtime.config.schedule_enabled
                || !Installer::is_navidrome_installed(&runtime.config)
            {
                continue;
            }
            let binary = Installer::navidrome_bin_path(&runtime.config);
            let rpc_binary = PluginManager::discord_rpc_bin_path(&runtime.config);
            let (navidrome, _) = runtime
                .processes
                .find_managed_processes(&binary, &rpc_binary);
            let should_run = config::schedule_is_active(&runtime.config, local_minutes());
            if should_run && navidrome.is_none() {
                if let Err(error) = state.installer.start_navidrome(&runtime.config) {
                    tracing::error!("Scheduled Navidrome start failed: {error:#}");
                }
            } else if !should_run && navidrome.is_some() {
                if let Err(error) = runtime.processes.stop_navidrome_at(&binary) {
                    tracing::error!("Scheduled Navidrome stop failed: {error:#}");
                }
            }
        }
    });
}

fn main() {
    webview_compat::apply_linux_webview_env();
    init_logging();
    children::start_reaper();
    utils::start_memory_maintenance();

    match service_window_args() {
        Ok(Some((service, host, port))) => {
            if let Err(error) = run_service_window(service, host, port) {
                tracing::error!("{error}");
                std::process::exit(1);
            }
            return;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::error!("{error}");
            std::process::exit(2);
        }
    }

    // A fully native Navidrome client (no embedded browser engine).
    #[cfg(target_os = "linux")]
    if std::env::args()
        .skip(1)
        .any(|arg| arg == "--syncpulse-client")
    {
        if let Err(error) = native_client::run() {
            tracing::error!("{error}");
            std::process::exit(1);
        }
        return;
    }

    desktop::set_linux_application_id("org.navidromehub.syncpulse");
    let mut context = tauri::generate_context!();
    // For AppImage launches current_exe points into a transient mount and its
    // path changes between launches. Register desktop shortcuts against the
    // original AppImage path instead, so the window app IDs resolve to the
    // matching entries in the compositor.
    let desktop_executable = std::env::var_os("APPIMAGE")
        .map(std::path::PathBuf::from)
        .map(Ok)
        .unwrap_or_else(std::env::current_exe);
    match desktop_executable {
        Ok(executable) => {
            let config = config::load();
            if let Err(error) = desktop::ensure_linux_desktop_integration(
                &executable,
                config.navidrome_port,
                config.invidious_port,
            ) {
                tracing::warn!("Unable to register desktop integration: {error}");
            }
        }
        Err(error) => {
            tracing::warn!("Unable to locate the executable for desktop integration: {error}");
        }
    }
    let icon = Image::from_bytes(include_bytes!("../icons/syncpulse.png"))
        .expect("SyncPulse icon must be a valid PNG");
    context.set_default_window_icon(Some(icon));
    tauri::Builder::default()
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            dashboard,
            system_metrics,
            select_directory,
            complete_initial_setup,
            install_navidrome,
            start_navidrome,
            stop_navidrome,
            download_audio,
            rescan_navidrome,
            install_discord_rpc,
            get_discord_rpc_config,
            save_discord_rpc_settings,
            restart_discord_rpc,
            enable_artwork_funnel,
            install_system_dependencies,
            operate_invidious,
            get_invidious_compose,
            save_invidious_compose,
            validate_invidious_compose,
            save_settings,
            open_navidrome,
            open_invidious,
            open_navidrome_client,
            open_navidrome_browser,
            open_invidious_browser
        ])
        .setup(|app| {
            let main_window = app
                .get_webview_window("main")
                .ok_or("SyncPulse main window was not found")?;
            let icon = Image::from_bytes(include_bytes!("../icons/syncpulse.png"))?;
            main_window.set_icon(icon)?;
            #[cfg(target_os = "linux")]
            main_window
                .with_webview(|webview| {
                    webkit_governor::tune_webview(
                        &webview.inner(),
                        webkit_governor::WebKitRole::ControlUi,
                    );
                })
                .map_err(std::io::Error::other)?;
            start_schedule_loop(app.handle().clone());
            Ok(())
        })
        .build(context)
        .expect("Failed to build SyncPulse")
        .run(|app_handle, event| match event {
            tauri::RunEvent::Ready => {
                let state = app_handle.state::<AppState>();
                let config = state.runtime.lock().map(|runtime| runtime.config.clone());
                match config {
                    Ok(config)
                        if config.setup_completed
                            && config.discord_rpc_enabled
                            && PluginManager::is_discord_rpc_installed(&config) =>
                    {
                        let plugins = state.plugins.clone();
                        if let Err(error) = plugins.start_discord_rpc_if_enabled(&config) {
                            tracing::error!("Unable to start Discord RPC when SyncPulse became ready: {error:#}");
                        }
                    }
                    Ok(_) => {}
                    Err(_) => tracing::error!("Application state lock was poisoned during Discord RPC startup"),
                }
            }
            tauri::RunEvent::Exit => {
                let state = app_handle.state::<AppState>();
                let config = state.runtime.lock().map(|runtime| runtime.config.clone());
                if let Ok(config) = config {
                    if let Err(error) = state.plugins.stop_managed_discord_rpc(&config) {
                        tracing::warn!("Unable to stop managed Discord RPC during SyncPulse shutdown: {error:#}");
                    }
                };
            }
            _ => {}
        });
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let log_path = config::config_dir().join("syncpulse.log");
    match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(file) => {
            let writer = SharedLogWriter(Arc::new(Mutex::new(file)));
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().with_writer(std::io::stderr))
                .with(fmt::layer().with_ansi(false).with_writer(writer))
                .init();
            tracing::info!("Logging to {}", log_path.display());
        }
        Err(error) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().with_writer(std::io::stderr))
                .init();
            tracing::warn!("Unable to open log file {}: {error}", log_path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_funnel_url, navidrome_status, system_operation_is_allowlisted};

    #[test]
    fn privileged_system_operations_are_fixed_and_package_allowlisted() {
        assert!(system_operation_is_allowlisted(
            "/usr/bin/pacman",
            &["-S", "--needed", "--noconfirm", "podman", "tailscale"]
        ));
        assert!(system_operation_is_allowlisted(
            "/usr/bin/apt-get",
            &["update"]
        ));
        assert!(system_operation_is_allowlisted(
            "/usr/bin/apt-get",
            &["install", "-y", "podman", "podman-compose"]
        ));
        assert!(system_operation_is_allowlisted(
            "/usr/bin/dnf",
            &["install", "-y", "podman"]
        ));
        assert!(system_operation_is_allowlisted(
            "/usr/bin/systemctl",
            &["enable", "--now", "tailscaled.service"]
        ));

        assert!(!system_operation_is_allowlisted(
            "/usr/bin/pacman",
            &["-S", "--needed", "--noconfirm", "docker"]
        ));
        assert!(!system_operation_is_allowlisted(
            "/usr/bin/apt-get",
            &["install", "-y", "podman", "tailscale"]
        ));
        assert!(!system_operation_is_allowlisted(
            "/usr/bin/systemctl",
            &["enable", "--now", "docker.service"]
        ));
        assert!(!system_operation_is_allowlisted(
            "/usr/bin/sh",
            &["-c", "id"]
        ));
    }

    #[test]
    fn responding_service_outside_syncpulse_is_never_called_missing_or_stopped() {
        let (status, detail) = navidrome_status(true, false, false);
        assert_eq!(status, "RESPONDING");
        assert!(detail.contains("outside SyncPulse"));
    }

    #[test]
    fn managed_binary_without_response_is_stopped_not_missing() {
        let (status, _) = navidrome_status(false, false, true);
        assert_eq!(status, "STOPPED");
    }

    #[test]
    fn managed_process_is_running_only_when_http_is_reachable() {
        assert_eq!(navidrome_status(true, true, true).0, "RUNNING");
        assert_eq!(navidrome_status(false, true, true).0, "PROCESS ONLY");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn funnel_url_extraction_matches_selected_https_port() {
        assert_eq!(
            extract_funnel_url(
                "Available on the internet: https://listen.example-tailnet.ts.net:8443",
                8443
            ),
            Some("https://listen.example-tailnet.ts.net:8443".to_owned())
        );
        assert_eq!(
            extract_funnel_url("https://listen.example-tailnet.ts.net:10000", 8443),
            None
        );
        assert_eq!(
            extract_funnel_url("https://listen.example-tailnet.ts.net", 443),
            Some("https://listen.example-tailnet.ts.net".to_owned())
        );
    }
}
