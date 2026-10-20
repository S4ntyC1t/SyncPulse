use std::fs::{self, File, OpenOptions};
use std::io::{copy, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use reqwest::Client;
use serde::{Deserialize, Serialize};
#[cfg(not(target_os = "linux"))]
use sysinfo::{ProcessesToUpdate, System};
use tar::Archive;
use url::Url;
use zip::ZipArchive;

use crate::config::Config;
use crate::utils::{detect_os_arch, is_windows, OsArch};

#[derive(Debug, Clone)]
pub struct PluginProgress {
    pub percent: f32,
    pub message: String,
}

#[derive(Clone)]
pub struct PluginManager {
    client: Client,
    rpc_supervisors: Arc<Mutex<std::collections::HashMap<PathBuf, Sender<RpcSupervisorCommand>>>>,
}

#[derive(Clone, Copy)]
enum RpcSupervisorCommand {
    Check,
    Restart,
    Stop,
}

impl Default for PluginManager {
    fn default() -> Self {
        Self {
            client: Client::builder()
                .user_agent("syncpulse/0.1.0")
                .timeout(Duration::from_secs(300))
                .build()
                .expect("Failed to build reqwest client"),
            rpc_supervisors: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    assets: Vec<GhAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DiscordRpcConfig {
    pub navidrome_url: String,
    pub navidrome_user: String,
    pub navidrome_password: String,
    pub cover_mode: CoverMode,
    pub navidrome_public_url: String,
    pub discord_client_id: String,
    pub poll_interval: u64,
    pub covers_listen: String,
}

impl Default for DiscordRpcConfig {
    fn default() -> Self {
        Self {
            navidrome_url: "http://127.0.0.1:4533".to_owned(),
            navidrome_user: String::new(),
            navidrome_password: String::new(),
            cover_mode: CoverMode::Off,
            navidrome_public_url: String::new(),
            discord_client_id: String::new(),
            poll_interval: 1,
            covers_listen: "127.0.0.1:4534".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CoverMode {
    Off,
    Navidrome,
    Server,
}

impl PluginManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn discord_rpc_bin_name() -> &'static str {
        if is_windows() {
            "navidrome-discord-rpc.exe"
        } else {
            "navidrome-discord-rpc"
        }
    }

    pub fn discord_rpc_bin_path(config: &Config) -> PathBuf {
        config.plugins_dir.join(Self::discord_rpc_bin_name())
    }

    pub fn discord_rpc_config_path(config: &Config) -> PathBuf {
        config.plugins_dir.join("navidrome-discord-rpc.toml")
    }

    pub fn is_discord_rpc_installed(config: &Config) -> bool {
        Self::discord_rpc_bin_path(config).is_file()
    }

    pub async fn install_discord_rpc(
        &self,
        config: &mut Config,
        progress: Option<&Sender<PluginProgress>>,
    ) -> Result<PathBuf> {
        fs::create_dir_all(&config.plugins_dir)?;
        let release = self
            .fetch_latest_release("S4ntyC1t", "navidrome-discord-rpc")
            .await?;
        let platform = supported_platform()?;
        let asset = find_asset(&release.assets, platform)
            .ok_or_else(|| anyhow!("No Discord RPC release asset for {platform}"))?;
        let archive = download_asset(&self.client, &asset, progress).await?;
        let binary = Self::discord_rpc_bin_path(config);
        extract_binary(&archive, &binary, platform)?;

        let rpc_config_path = Self::discord_rpc_config_path(config);
        if !rpc_config_path.exists() {
            write_discord_config(
                config,
                &DiscordRpcConfig {
                    navidrome_url: format!("http://127.0.0.1:{}", config.navidrome_port),
                    ..DiscordRpcConfig::default()
                },
            )?;
        }
        config.discord_rpc_bin = Some(binary.clone());
        config.discord_rpc_config = Some(rpc_config_path);
        config.discord_rpc_enabled = false;
        crate::config::save(config).context("Unable to save Discord RPC configuration")?;
        send_progress(
            progress,
            100.0,
            "Discord RPC installed; complete its local settings before enabling it",
        );
        Ok(binary)
    }

    pub fn start_discord_rpc_if_enabled(&self, config: &Config) -> Result<()> {
        if !config.discord_rpc_enabled {
            return Ok(());
        }
        let Some(binary) = config.discord_rpc_bin.as_ref() else {
            return Ok(());
        };
        if !binary.is_file() {
            return Err(anyhow!(
                "Discord RPC binary not found at {}",
                binary.display()
            ));
        }
        if !cfg!(target_os = "linux") {
            return Err(anyhow!(
                "The adapted Discord RPC integration is currently supported on Linux only"
            ));
        }
        let binary = binary.canonicalize().with_context(|| {
            format!(
                "Unable to resolve Discord RPC binary at {}",
                binary.display()
            )
        })?;
        let working_dir = config
            .plugins_dir
            .canonicalize()
            .with_context(|| format!("Unable to resolve {}", config.plugins_dir.display()))?;
        let settings = Self::load_discord_rpc_config(config)?;
        Self::validate_discord_rpc_config(&settings, true)?;
        tracing::info!("Starting SyncPulse-owned Discord Rich Presence supervisor");
        if settings.cover_mode == CoverMode::Server {
            let listener: std::net::SocketAddr = settings
                .covers_listen
                .parse()
                .context("Cover server address must look like 127.0.0.1:4534")?;
            if !Self::is_artwork_server_running_at(&binary, listener) {
                let covers_env = [
                    ("COVERS_LISTEN".to_owned(), settings.covers_listen.clone()),
                    ("NAVIDROME_URL".to_owned(), settings.navidrome_url.clone()),
                ];
                Self::spawn_cover_server(&binary, &working_dir, &covers_env)?;
            }
        }
        {
            let mut supervisors = self
                .rpc_supervisors
                .lock()
                .map_err(|_| anyhow!("Discord RPC supervisor registry lock was poisoned"))?;
            if let Some(tx) = supervisors.get(&binary) {
                if tx.send(RpcSupervisorCommand::Check).is_ok() {
                    return Ok(());
                }
                supervisors.remove(&binary);
            }
        }
        let env = Self::discord_rpc_environment(&settings);
        let tx =
            self.start_discord_rpc_supervisor(config.clone(), binary.clone(), working_dir, env)?;
        self.rpc_supervisors
            .lock()
            .map_err(|_| anyhow!("Discord RPC supervisor registry lock was poisoned"))?
            .insert(binary, tx);
        Ok(())
    }

    pub fn restart_managed_discord_rpc(&self, config: &Config) -> Result<()> {
        let binary = config
            .discord_rpc_bin
            .as_ref()
            .ok_or_else(|| anyhow!("Discord RPC binary is not configured"))?
            .canonicalize()
            .context("Unable to resolve Discord RPC binary")?;
        if linux_plugin_process_running(&binary, None) {
            return Err(anyhow!(
                "The Discord Rich Presence process managed by SyncPulse is already running"
            ));
        }
        {
            let mut supervisors = self
                .rpc_supervisors
                .lock()
                .map_err(|_| anyhow!("Discord RPC supervisor registry lock was poisoned"))?;
            if let Some(tx) = supervisors.get(&binary) {
                if tx.send(RpcSupervisorCommand::Restart).is_ok() {
                    return Ok(());
                }
                supervisors.remove(&binary);
            }
        }
        self.start_discord_rpc_if_enabled(config)
    }

    pub fn stop_managed_discord_rpc(&self, config: &Config) -> Result<bool> {
        let Some(binary) = config.discord_rpc_bin.as_ref() else {
            return Ok(false);
        };
        let binary = binary.canonicalize().unwrap_or_else(|_| binary.clone());
        let tx = self
            .rpc_supervisors
            .lock()
            .map_err(|_| anyhow!("Discord RPC supervisor registry lock was poisoned"))?
            .remove(&binary);
        if let Some(tx) = tx {
            let _ = tx.send(RpcSupervisorCommand::Stop);
            #[cfg(target_os = "linux")]
            {
                let deadline = Instant::now() + Duration::from_secs(3);
                while linux_plugin_process_running(&binary, None) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if linux_plugin_process_running(&binary, None) {
                    return Err(anyhow!("Managed Discord RPC did not stop within 3 seconds"));
                }
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn start_discord_rpc_supervisor(
        &self,
        config: Config,
        binary: PathBuf,
        working_dir: PathBuf,
        environment: Vec<(String, String)>,
    ) -> Result<Sender<RpcSupervisorCommand>> {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("discord-rpc-supervisor".to_owned())
            .spawn(move || {
                supervise_discord_rpc(config, binary, working_dir, environment, rx);
            })
            .context("Unable to start Discord RPC supervisor thread")?;
        Ok(tx)
    }

    pub fn discord_rpc_conflict(config: &Config) -> Option<String> {
        let binary = config.discord_rpc_bin.as_ref()?.canonicalize().ok()?;
        let (pid, executable) = Self::find_other_discord_rpc_process(&binary)?;
        Some(format!(
            "Another Discord RPC copy is running (PID {pid}: {}). Close that copy before SyncPulse can own Discord Rich Presence.",
            executable.display()
        ))
    }

    pub fn start_artwork_server(&self, config: &Config, listen: &str) -> Result<()> {
        let listener: std::net::SocketAddr = listen
            .parse()
            .context("Cover server address must look like 127.0.0.1:4534")?;
        if !listener.ip().is_loopback() {
            return Err(anyhow!(
                "The artwork server must bind to a loopback address"
            ));
        }
        let binary = config
            .discord_rpc_bin
            .as_ref()
            .ok_or_else(|| anyhow!("Install Discord Rich Presence before setting up artwork"))?
            .canonicalize()
            .context("Unable to find the Discord Rich Presence binary")?;
        let working_dir = config
            .plugins_dir
            .canonicalize()
            .with_context(|| format!("Unable to resolve {}", config.plugins_dir.display()))?;
        if Self::is_artwork_server_running_at(&binary, listener) {
            return Ok(());
        }
        let settings = Self::load_discord_rpc_config(config)?;
        let environment = [
            ("COVERS_LISTEN".to_owned(), listener.to_string()),
            ("NAVIDROME_URL".to_owned(), settings.navidrome_url),
        ];
        let log_path = config.plugins_dir.join("discord-artwork-server.log");
        let log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&log_path)
            .with_context(|| format!("Unable to open {}", log_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            log.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let stderr = log.try_clone()?;
        let mut child = Command::new(&binary)
            .arg("serve-covers")
            .current_dir(&working_dir)
            .envs(environment.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .spawn()
            .context("Unable to spawn the local artwork server")?;

        for _ in 0..20 {
            if let Some(status) = child
                .try_wait()
                .context("Unable to check the local artwork server process")?
            {
                let mut output = String::new();
                File::open(&log_path)
                    .and_then(|mut file| file.read_to_string(&mut output))
                    .ok();
                let output = output.trim();
                return Err(anyhow!(
                    "The artwork server exited with {status}.{} See {}",
                    if output.is_empty() {
                        String::new()
                    } else {
                        format!(" Output: {output}.")
                    },
                    log_path.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    async fn fetch_latest_release(&self, owner: &str, repo: &str) -> Result<GhRelease> {
        self.client
            .get(format!(
                "https://api.github.com/repos/{owner}/{repo}/releases/latest"
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Unable to read latest GitHub release")
    }

    pub fn load_discord_rpc_config(config: &Config) -> Result<DiscordRpcConfig> {
        let path = PluginManager::discord_rpc_config_path(config);
        if !path.exists() {
            return Ok(DiscordRpcConfig {
                navidrome_url: format!("http://127.0.0.1:{}", config.navidrome_port),
                ..DiscordRpcConfig::default()
            });
        }
        let text = fs::read_to_string(&path).with_context(|| {
            format!("Unable to read Discord RPC settings at {}", path.display())
        })?;
        let mut settings: DiscordRpcConfig = toml::from_str(&text).with_context(|| {
            format!("Unable to parse Discord RPC settings at {}", path.display())
        })?;
        Self::normalize_discord_rpc_config(&mut settings);
        Ok(settings)
    }

    pub fn save_discord_rpc_config(config: &Config, settings: &DiscordRpcConfig) -> Result<()> {
        let mut settings = settings.clone();
        Self::normalize_discord_rpc_config(&mut settings);
        Self::validate_discord_rpc_config(&settings, false)?;
        fs::create_dir_all(&config.plugins_dir)?;
        write_discord_config(config, &settings)
    }

    fn normalize_discord_rpc_config(settings: &mut DiscordRpcConfig) {
        if settings.covers_listen.trim().is_empty() {
            settings.covers_listen = "127.0.0.1:4534".to_owned();
        }
    }

    pub fn validate_discord_rpc_config(
        settings: &DiscordRpcConfig,
        require_credentials: bool,
    ) -> Result<()> {
        Self::validate_http_url(&settings.navidrome_url, "Navidrome URL")?;
        if !settings.navidrome_user.trim().is_empty() && settings.navidrome_password.is_empty()
            || settings.navidrome_user.trim().is_empty() && !settings.navidrome_password.is_empty()
        {
            return Err(anyhow!("Enter both the Navidrome username and password"));
        }
        if require_credentials
            && (settings.navidrome_user.trim().is_empty() || settings.navidrome_password.is_empty())
        {
            return Err(anyhow!(
                "Enter the Navidrome username and password before enabling Discord Rich Presence"
            ));
        }
        if settings.poll_interval == 0 || settings.poll_interval > 3600 {
            return Err(anyhow!("Poll interval must be between 1 and 3600 seconds"));
        }
        if !settings.discord_client_id.is_empty()
            && !settings
                .discord_client_id
                .chars()
                .all(|character| character.is_ascii_digit())
        {
            return Err(anyhow!("Discord Application ID must contain digits only"));
        }
        if settings.cover_mode != CoverMode::Off {
            Self::validate_https_url(&settings.navidrome_public_url, "Public artwork URL")?;
        }
        if settings.cover_mode == CoverMode::Server {
            let listener: std::net::SocketAddr = settings
                .covers_listen
                .parse()
                .context("Cover server address must look like 127.0.0.1:4534")?;
            if !listener.ip().is_loopback() {
                return Err(anyhow!("The cover server must bind to a loopback address; publish it through an explicit tunnel or reverse proxy"));
            }
        }
        Ok(())
    }

    fn validate_http_url(value: &str, label: &str) -> Result<()> {
        let parsed =
            Url::parse(value.trim()).with_context(|| format!("{label} must be a valid URL"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(anyhow!(
                "{label} must use http:// or https:// and include a host"
            ));
        }
        Ok(())
    }

    fn validate_https_url(value: &str, label: &str) -> Result<()> {
        if value.trim().is_empty() {
            return Err(anyhow!(
                "Enter a public HTTPS artwork URL, or set Album Art Source to Off"
            ));
        }
        Self::validate_http_url(value, label)?;
        if Url::parse(value.trim())?.scheme() != "https" {
            return Err(anyhow!(
                "{label} must be a public HTTPS address (for example, your Tailscale Funnel or Cloudflare hostname), not the local HTTP cover-server address"
            ));
        }
        Ok(())
    }

    fn discord_rpc_environment(settings: &DiscordRpcConfig) -> Vec<(String, String)> {
        let mut environment = vec![
            ("NAVIDROME_URL".to_owned(), settings.navidrome_url.clone()),
            ("NAVIDROME_USER".to_owned(), settings.navidrome_user.clone()),
            (
                "NAVIDROME_PASSWORD".to_owned(),
                settings.navidrome_password.clone(),
            ),
            (
                "COVER_MODE".to_owned(),
                match settings.cover_mode {
                    CoverMode::Off => "off",
                    CoverMode::Navidrome => "navidrome",
                    CoverMode::Server => "server",
                }
                .to_owned(),
            ),
            (
                "POLL_INTERVAL".to_owned(),
                settings.poll_interval.to_string(),
            ),
        ];
        if settings.cover_mode != CoverMode::Off && !settings.navidrome_public_url.is_empty() {
            environment.push((
                "NAVIDROME_PUBLIC_URL".to_owned(),
                settings.navidrome_public_url.clone(),
            ));
        }
        if !settings.discord_client_id.is_empty() {
            environment.push((
                "DISCORD_CLIENT_ID".to_owned(),
                settings.discord_client_id.clone(),
            ));
        }
        environment
    }

    #[cfg(not(target_os = "linux"))]
    fn is_plugin_process_running(binary: &Path, required_argument: Option<&str>) -> bool {
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        system.processes().values().any(|process| {
            let Some(executable) = process.exe() else {
                return false;
            };
            if !Self::same_path(executable, binary) {
                return false;
            }
            let is_cover_server = Self::process_is_cover_server(process);
            match required_argument {
                Some("serve-covers") => is_cover_server,
                Some(_) => false,
                None => !is_cover_server,
            }
        })
    }

    fn find_other_discord_rpc_process(binary: &Path) -> Option<(u32, PathBuf)> {
        #[cfg(target_os = "linux")]
        {
            return linux_other_discord_rpc_process(binary);
        }

        #[cfg(not(target_os = "linux"))]
        {
            let expected_name = binary.file_name()?;
            let mut system = System::new();
            system.refresh_processes(ProcessesToUpdate::All, true);
            system.processes().iter().find_map(|(pid, process)| {
                let executable = process.exe()?;
                if executable.file_name() != Some(expected_name)
                    || Self::same_path(executable, binary)
                    || Self::process_is_cover_server(process)
                {
                    return None;
                }
                Some((pid.as_u32(), executable.to_path_buf()))
            })
        }
    }

    fn is_artwork_server_running_at(binary: &Path, listener: std::net::SocketAddr) -> bool {
        use std::net::TcpStream;

        #[cfg(target_os = "linux")]
        let found = linux_artwork_server_running_at(binary, listener);
        #[cfg(not(target_os = "linux"))]
        let found = {
            let mut system = System::new();
            system.refresh_processes(ProcessesToUpdate::All, true);
            system.processes().values().any(|process| {
                let Some(executable) = process.exe() else {
                    return false;
                };
                if executable.file_name() != binary.file_name()
                    || !Self::process_is_cover_server(process)
                {
                    return false;
                }
                let listens_here = process.environ().iter().any(|entry| {
                    entry
                        .to_string_lossy()
                        .strip_prefix("COVERS_LISTEN=")
                        .is_some_and(|value| value == listener.to_string())
                });
                listens_here
            })
        };
        found && TcpStream::connect_timeout(&listener, Duration::from_millis(200)).is_ok()
    }

    #[cfg(not(target_os = "linux"))]
    fn process_is_cover_server(process: &sysinfo::Process) -> bool {
        process
            .cmd()
            .iter()
            .any(|argument| argument.to_string_lossy() == "serve-covers")
            || process
                .environ()
                .iter()
                .any(|entry| entry.to_string_lossy().starts_with("COVERS_LISTEN="))
    }

    fn spawn_cover_server(
        binary: &Path,
        working_dir: &Path,
        environment: &[(String, String)],
    ) -> Result<()> {
        Self::spawn_logged_service(
            binary,
            &["serve-covers"],
            working_dir,
            environment,
            "discord-artwork-server.log",
            "Private artwork server",
        )
    }

    fn spawn_logged_service(
        binary: &Path,
        arguments: &[&str],
        working_dir: &Path,
        environment: &[(String, String)],
        log_name: &str,
        service_name: &str,
    ) -> Result<()> {
        let log_path = working_dir.join(log_name);
        let log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&log_path)
            .with_context(|| format!("Unable to open {}", log_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            log.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let stderr = log.try_clone()?;
        let mut command = Command::new(binary);
        command
            .args(arguments)
            .current_dir(working_dir)
            .envs(environment.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        set_parent_death_signal(&mut command);
        let mut child = command
            .spawn()
            .context("Unable to spawn Discord Rich Presence")?;

        for _ in 0..30 {
            if let Some(status) = child
                .try_wait()
                .context("Unable to check the Discord Rich Presence process")?
            {
                let mut output = String::new();
                File::open(&log_path)
                    .and_then(|mut file| file.read_to_string(&mut output))
                    .ok();
                let output = output.trim();
                return Err(anyhow!(
                    "{service_name} exited with {status}.{} See {}",
                    if output.is_empty() {
                        String::new()
                    } else {
                        format!(" Output: {output}.")
                    },
                    log_path.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // Dropping std::process::Child does not terminate it; the process is
        // intentionally left running as a background integration.
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn same_path(left: &Path, right: &Path) -> bool {
        left.canonicalize().unwrap_or_else(|_| left.to_path_buf())
            == right.canonicalize().unwrap_or_else(|_| right.to_path_buf())
    }
}

#[cfg(target_os = "linux")]
fn linux_plugin_process_running(binary: &Path, required_argument: Option<&str>) -> bool {
    linux_processes()
        .into_iter()
        .any(|(_, executable, args, env)| {
            if !same_path(&executable, binary) {
                return false;
            }
            let is_cover_server = process_args_are_cover_server(&args, &env);
            match required_argument {
                Some("serve-covers") => is_cover_server,
                Some(_) => false,
                None => !is_cover_server,
            }
        })
}

#[cfg(target_os = "linux")]
fn linux_other_discord_rpc_process(binary: &Path) -> Option<(u32, PathBuf)> {
    let expected_name = binary.file_name()?;
    linux_processes()
        .into_iter()
        .find_map(|(pid, executable, args, env)| {
            (executable.file_name() == Some(expected_name)
                && !same_path(&executable, binary)
                && !process_args_are_cover_server(&args, &env))
            .then_some((pid, executable))
        })
}

#[cfg(target_os = "linux")]
fn linux_artwork_server_running_at(binary: &Path, listener: std::net::SocketAddr) -> bool {
    linux_processes()
        .into_iter()
        .any(|(_, executable, args, env)| {
            executable.file_name() == binary.file_name()
                && process_args_are_cover_server(&args, &env)
                && env.iter().any(|entry| {
                    entry
                        .strip_prefix("COVERS_LISTEN=")
                        .is_some_and(|value| value == listener.to_string())
                })
        })
}

#[cfg(target_os = "linux")]
fn linux_processes() -> Vec<(u32, PathBuf, Vec<String>, Vec<String>)> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let root = entry.path();
            let stat = fs::read_to_string(root.join("stat")).ok()?;
            let close_comm = stat.rfind(')')?;
            if stat[close_comm + 1..].split_whitespace().next() == Some("Z") {
                return None;
            }
            let executable = fs::read_link(root.join("exe")).ok()?;
            let args = read_proc_strings(root.join("cmdline"));
            let env = read_proc_strings(root.join("environ"));
            Some((pid, executable, args, env))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn read_proc_strings(path: PathBuf) -> Vec<String> {
    fs::read(path)
        .unwrap_or_default()
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

#[cfg(target_os = "linux")]
fn spawn_rpc_child(
    binary: &Path,
    working_dir: &Path,
    environment: &[(String, String)],
) -> Result<Child> {
    let log_path = working_dir.join("discord-rpc.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Unable to open {}", log_path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    log.set_permissions(fs::Permissions::from_mode(0o600))?;
    let stderr = log.try_clone()?;
    let mut command = Command::new(binary);
    command
        .current_dir(working_dir)
        .envs(environment.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    set_parent_death_signal(&mut command);
    command
        .spawn()
        .context("Unable to spawn Discord Rich Presence")
}

#[cfg(target_os = "linux")]
fn set_parent_death_signal(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    let parent_pid = std::process::id() as libc::pid_t;
    // Keep managed helpers scoped to SyncPulse even if it is terminated
    // abruptly before its normal shutdown handler can stop them.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "SyncPulse exited before the managed helper started",
                ));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn set_parent_death_signal(_command: &mut Command) {}

#[cfg(target_os = "linux")]
fn supervise_discord_rpc(
    config: Config,
    binary: PathBuf,
    working_dir: PathBuf,
    environment: Vec<(String, String)>,
    commands: mpsc::Receiver<RpcSupervisorCommand>,
) {
    let mut child: Option<Child> = None;
    let mut ipc_socket: Option<(PathBuf, u64, u64)> = None;
    let mut consecutive_failures = 0_u32;
    let mut child_started_at: Option<Instant> = None;
    let mut retry_after: Option<Instant> = None;
    let mut warned_about_other_copy = false;
    loop {
        match commands.try_recv() {
            Ok(RpcSupervisorCommand::Stop) | Err(mpsc::TryRecvError::Disconnected) => {
                if let Some(mut child) = child {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return;
            }
            Ok(RpcSupervisorCommand::Restart) => {
                if let Some(mut running) = child.take() {
                    let _ = running.kill();
                    let _ = running.wait();
                }
                child_started_at = None;
                ipc_socket = None;
                consecutive_failures = 0;
                retry_after = None;
            }
            Ok(RpcSupervisorCommand::Check) => {}
            Err(mpsc::TryRecvError::Empty) => {}
        }

        let current_ipc_socket = match refresh_discord_ipc_runtime(&config) {
            Ok(_) => discord_ipc_socket_identity(),
            Err(error) => {
                tracing::warn!("Unable to refresh Discord IPC socket: {error:#}");
                None
            }
        };

        if should_restart_rpc_for_discord_socket(
            child.is_some(),
            ipc_socket.as_ref(),
            current_ipc_socket.as_ref(),
        ) {
            tracing::info!("Discord IPC socket changed; reconnecting Rich Presence bridge");
            if let Some(mut running) = child.take() {
                let _ = running.kill();
                let _ = running.wait();
            }
            child_started_at = None;
            ipc_socket = None;
        }

        if child.is_none() {
            let other_copy = PluginManager::find_other_discord_rpc_process(&binary);
            let same_binary_already_running = linux_plugin_process_running(&binary, None);
            if let Some((pid, executable)) = other_copy {
                if !warned_about_other_copy {
                    tracing::warn!("Discord RPC copy already running (PID {pid}: {}); leaving it untouched and waiting before starting SyncPulse's managed copy", executable.display());
                    warned_about_other_copy = true;
                }
                retry_after = Some(Instant::now() + Duration::from_secs(2));
            } else if same_binary_already_running {
                if !warned_about_other_copy {
                    tracing::warn!("A Discord RPC process using SyncPulse's configured binary is already running outside this supervisor; leaving it untouched and waiting");
                    warned_about_other_copy = true;
                }
                retry_after = Some(Instant::now() + Duration::from_secs(2));
            } else if retry_after.map_or(true, |retry| Instant::now() >= retry) {
                let mut launch_environment: Vec<_> = environment
                    .iter()
                    .filter(|(key, _)| key != "XDG_RUNTIME_DIR")
                    .cloned()
                    .collect();
                match refresh_discord_ipc_runtime(&config) {
                    Ok(Some(runtime)) => launch_environment
                        .push(("XDG_RUNTIME_DIR".to_owned(), runtime.display().to_string())),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!("Unable to prepare Discord IPC runtime: {error:#}");
                    }
                }
                match spawn_rpc_child(&binary, &working_dir, &launch_environment) {
                    Ok(started) => {
                        tracing::info!(
                            "Discord IPC is ready; started managed Rich Presence bridge"
                        );
                        child = Some(started);
                        child_started_at = Some(Instant::now());
                        ipc_socket = current_ipc_socket.clone();
                        warned_about_other_copy = false;
                        retry_after = None;
                        consecutive_failures = 0;
                    }
                    Err(error) => {
                        tracing::error!("Unable to start Discord Rich Presence: {error:#}");
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        retry_after = Some(
                            Instant::now()
                                + Duration::from_secs(u64::from(consecutive_failures.clamp(1, 10))),
                        );
                    }
                }
            }
        }

        let child_status = child.as_mut().map(Child::try_wait);
        if let Some(child_status) = child_status {
            match child_status {
                Ok(Some(status)) => {
                    if child_started_at
                        .is_some_and(|started| started.elapsed() >= Duration::from_secs(30))
                    {
                        consecutive_failures = 0;
                    }
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    tracing::error!("Discord Rich Presence exited with {status}; restarting when Discord is available (attempt {consecutive_failures})");
                    child = None;
                    child_started_at = None;
                    ipc_socket = None;
                    retry_after = Some(
                        Instant::now()
                            + Duration::from_secs(u64::from(consecutive_failures.clamp(1, 10))),
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!("Unable to monitor Discord Rich Presence child: {error}");
                    if let Some(mut running) = child.take() {
                        let _ = running.kill();
                        let _ = running.wait();
                    }
                    child = None;
                    child_started_at = None;
                    ipc_socket = None;
                    retry_after = Some(Instant::now() + Duration::from_secs(1));
                }
            }
        }

        match commands.recv_timeout(Duration::from_secs(1)) {
            Ok(RpcSupervisorCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(mut running) = child {
                    let _ = running.kill();
                    let _ = running.wait();
                }
                return;
            }
            Ok(RpcSupervisorCommand::Restart | RpcSupervisorCommand::Check)
            | Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(target_os = "linux")]
fn discord_ipc_socket_identity() -> Option<(PathBuf, u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    let socket = find_connectable_discord_socket()?;
    let metadata = fs::metadata(&socket).ok()?;
    Some((socket, metadata.dev(), metadata.ino()))
}

#[cfg(target_os = "linux")]
fn discord_ipc_socket_changed(
    previous: Option<&(PathBuf, u64, u64)>,
    current: Option<&(PathBuf, u64, u64)>,
) -> bool {
    current.is_some() && previous != current
}

#[cfg(target_os = "linux")]
fn should_restart_rpc_for_discord_socket(
    child_running: bool,
    previous: Option<&(PathBuf, u64, u64)>,
    current: Option<&(PathBuf, u64, u64)>,
) -> bool {
    child_running && discord_ipc_socket_changed(previous, current)
}

#[cfg(target_os = "linux")]
fn process_args_are_cover_server(args: &[String], env: &[String]) -> bool {
    args.iter().any(|argument| argument == "serve-covers")
        || env.iter().any(|entry| entry.starts_with("COVERS_LISTEN="))
}

#[cfg(target_os = "linux")]
fn same_path(left: &Path, right: &Path) -> bool {
    left.canonicalize().unwrap_or_else(|_| left.to_path_buf())
        == right.canonicalize().unwrap_or_else(|_| right.to_path_buf())
}

#[cfg(target_os = "linux")]
fn find_connectable_discord_socket() -> Option<PathBuf> {
    const ENV_KEYS: [&str; 4] = ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"];

    for key in ENV_KEYS {
        let Ok(base) = std::env::var(key) else {
            continue;
        };
        let base = PathBuf::from(base);
        if !base.is_dir() {
            continue;
        }
        if let Some(socket) = find_connectable_discord_socket_in(&base) {
            return Some(socket);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn find_connectable_discord_socket_in(base: &Path) -> Option<PathBuf> {
    use std::os::unix::net::UnixStream;

    const SUBPATHS: [&str; 5] = [
        "",
        "app/com.discordapp.Discord/",
        "app/dev.vencord.Vesktop/",
        ".flatpak/com.discordapp.Discord/xdg-run/",
        ".flatpak/dev.vencord.Vesktop/xdg-run/",
    ];
    for index in 0..10 {
        for subpath in SUBPATHS {
            let candidate = base.join(subpath).join(format!("discord-ipc-{index}"));
            if UnixStream::connect(&candidate).is_ok() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn refresh_discord_ipc_runtime(config: &Config) -> Result<Option<PathBuf>> {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let runtime_dir = config.plugins_dir.join(".discord-ipc-runtime");
    fs::create_dir_all(&runtime_dir).with_context(|| {
        format!(
            "Unable to create Discord IPC runtime directory at {}",
            runtime_dir.display()
        )
    })?;
    fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))
        .context("Unable to restrict Discord IPC runtime directory permissions")?;
    let ipc_link = runtime_dir.join("discord-ipc-0");
    let Some(socket) = find_connectable_discord_socket() else {
        match fs::symlink_metadata(&ipc_link) {
            Ok(metadata) if metadata.file_type().is_symlink() => fs::remove_file(&ipc_link)
                .context("Unable to remove stale managed Discord IPC link")?,
            Ok(_) => {
                return Err(anyhow!(
                    "Refusing to replace unexpected file at {}",
                    ipc_link.display()
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Unable to inspect Discord IPC link"),
        }
        return Ok(Some(runtime_dir));
    };
    let current_target = fs::read_link(&ipc_link).ok();
    if current_target.as_ref() != Some(&socket) {
        match fs::symlink_metadata(&ipc_link) {
            Ok(metadata) if !metadata.file_type().is_symlink() => {
                return Err(anyhow!(
                    "Refusing to replace unexpected file at {}",
                    ipc_link.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Unable to inspect Discord IPC link"),
        }
        let temp_link = runtime_dir.join(format!("discord-ipc-0.{}.tmp", std::process::id()));
        match fs::symlink_metadata(&temp_link) {
            Ok(metadata) if metadata.file_type().is_symlink() => fs::remove_file(&temp_link)?,
            Ok(_) => {
                return Err(anyhow!(
                    "Refusing to replace unexpected file at {}",
                    temp_link.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context("Unable to inspect temporary Discord IPC link")
            }
        }
        symlink(&socket, &temp_link)
            .with_context(|| format!("Unable to point Discord IPC link at {}", socket.display()))?;
        match fs::rename(&temp_link, &ipc_link) {
            Ok(()) => {}
            Err(error) => {
                return Err(error).context("Unable to atomically update Discord IPC link")
            }
        }
    }
    Ok(Some(runtime_dir))
}

#[cfg(not(target_os = "linux"))]
fn refresh_discord_ipc_runtime(_config: &Config) -> Result<Option<PathBuf>> {
    Ok(None)
}

fn supported_platform() -> Result<OsArch> {
    let platform = detect_os_arch();
    match platform {
        OsArch::LinuxX86_64 | OsArch::LinuxAArch64 => Ok(platform),
        OsArch::Unknown => Err(anyhow!(
            "Plugin installation is not supported on this platform"
        )),
        _ => Err(anyhow!("The upstream Discord RPC integration supports Unix-like systems; SyncPulse currently packages it for Linux only")),
    }
}

fn find_asset(assets: &[GhAsset], platform: OsArch) -> Option<GhAsset> {
    assets
        .iter()
        .find(|asset| {
            let name = asset.name.to_ascii_lowercase();
            let archive = name.ends_with(".tar.gz") || name.ends_with(".zip");
            let os_matches = match platform {
                OsArch::LinuxX86_64 | OsArch::LinuxAArch64 => name.contains("linux"),
                OsArch::WindowsX86_64 | OsArch::WindowsAArch64 => name.contains("windows"),
                OsArch::Unknown => false,
            };
            let arch_matches = match platform {
                OsArch::LinuxX86_64 | OsArch::WindowsX86_64 => {
                    name.contains("amd64") || name.contains("x86_64")
                }
                OsArch::LinuxAArch64 | OsArch::WindowsAArch64 => {
                    name.contains("arm64") || name.contains("aarch64")
                }
                OsArch::Unknown => false,
            };
            archive && os_matches && arch_matches
        })
        .cloned()
}

async fn download_asset(
    client: &Client,
    asset: &GhAsset,
    progress: Option<&Sender<PluginProgress>>,
) -> Result<tempfile::TempPath> {
    let mut response = client
        .get(&asset.browser_download_url)
        .send()
        .await?
        .error_for_status()?;
    let suffix = if asset.name.to_ascii_lowercase().ends_with(".tar.gz") {
        ".tar.gz"
    } else {
        ".zip"
    };
    let mut file = tempfile::Builder::new().suffix(suffix).tempfile()?;
    let length = response.content_length();
    let mut downloaded = 0_u64;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk)?;
        downloaded += chunk.len() as u64;
        let percent = length
            .filter(|length| *length > 0)
            .map(|length| 15.0 + 70.0 * (downloaded as f32 / length as f32).min(1.0))
            .unwrap_or(15.0);
        send_progress(progress, percent, "Downloading plugin release");
    }
    if downloaded == 0 {
        return Err(anyhow!("Downloaded plugin archive is empty"));
    }
    file.as_file().sync_all()?;
    Ok(file.into_temp_path())
}

fn extract_binary(archive_path: &Path, destination: &Path, platform: OsArch) -> Result<()> {
    let windows = matches!(platform, OsArch::WindowsX86_64 | OsArch::WindowsAArch64);
    let parent = destination
        .parent()
        .context("Plugin binary path has no parent directory")?;
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    let mut found = false;

    let archive_name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if archive_name.ends_with(".zip") {
        let mut archive = ZipArchive::new(File::open(archive_path)?)?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            if !entry.is_file() || is_documentation_file(entry.name()) {
                continue;
            }
            let executable = Path::new(entry.name())
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"));
            let unix_executable = entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
            if (windows && executable) || (!windows && unix_executable) {
                copy(&mut entry, output.as_file_mut())?;
                found = true;
                break;
            }
        }
    } else if archive_name.ends_with(".gz") {
        let decoder = GzDecoder::new(File::open(archive_path)?);
        let mut archive = Archive::new(decoder);
        for entry in archive.entries()? {
            let mut entry = entry?;
            if !entry.header().entry_type().is_file()
                || is_documentation_file(&entry.path()?.to_string_lossy())
                || entry.header().mode().is_ok_and(|mode| mode & 0o111 == 0)
            {
                continue;
            }
            copy(&mut entry, output.as_file_mut())?;
            found = true;
            break;
        }
    } else {
        return Err(anyhow!(
            "Unsupported plugin archive: {}",
            archive_path.display()
        ));
    }
    if !found {
        return Err(anyhow!("No executable found in plugin release archive"));
    }
    if output.as_file().metadata()?.len() == 0 {
        return Err(anyhow!("Extracted plugin executable is empty"));
    }
    output.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        output
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    output
        .persist(destination)
        .with_context(|| format!("Unable to install plugin at {}", destination.display()))?;
    Ok(())
}

fn is_documentation_file(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name.starts_with("readme")
        || name.starts_with("license")
        || name.starts_with("copying")
        || name.starts_with("notice")
}

fn write_discord_config(config: &Config, plugin_config: &DiscordRpcConfig) -> Result<()> {
    let path = PluginManager::discord_rpc_config_path(config);
    let mut temporary = tempfile::NamedTempFile::new_in(&config.plugins_dir)?;
    temporary.write_all(toml::to_string_pretty(&plugin_config)?.as_bytes())?;
    temporary.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary
        .persist(&path)
        .with_context(|| format!("Unable to write plugin config at {}", path.display()))?;
    Ok(())
}

fn send_progress(progress: Option<&Sender<PluginProgress>>, percent: f32, message: &str) {
    if let Some(progress) = progress {
        let _ = progress.send(PluginProgress {
            percent,
            message: message.to_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        discord_ipc_socket_changed, find_asset, find_connectable_discord_socket_in,
        should_restart_rpc_for_discord_socket, CoverMode, DiscordRpcConfig, GhAsset, PluginManager,
    };
    use crate::config::Config;
    use crate::utils::OsArch;

    #[test]
    fn plugin_release_asset_must_match_os_and_architecture() {
        let assets = vec![
            GhAsset {
                name: "linux-amd64.zip".to_owned(),
                browser_download_url: String::new(),
            },
            GhAsset {
                name: "windows-arm64.zip".to_owned(),
                browser_download_url: String::new(),
            },
        ];
        assert_eq!(
            find_asset(&assets, OsArch::LinuxX86_64).map(|asset| asset.name),
            Some("linux-amd64.zip".to_owned())
        );
        assert!(find_asset(&assets, OsArch::LinuxAArch64).is_none());
    }

    #[test]
    fn discord_rpc_configuration_requires_credentials_and_public_https_for_covers() {
        let mut settings = DiscordRpcConfig {
            navidrome_user: "alice".to_owned(),
            navidrome_password: "secret".to_owned(),
            cover_mode: CoverMode::Server,
            navidrome_public_url: "https://music.example.ts.net:8443".to_owned(),
            ..DiscordRpcConfig::default()
        };
        assert!(PluginManager::validate_discord_rpc_config(&settings, true).is_ok());

        settings.navidrome_public_url.clear();
        let error = PluginManager::validate_discord_rpc_config(&settings, true).unwrap_err();
        assert!(error
            .to_string()
            .contains("Enter a public HTTPS artwork URL"));

        settings.navidrome_public_url = "http://music.example.test".to_owned();
        let error = PluginManager::validate_discord_rpc_config(&settings, true).unwrap_err();
        assert!(error.to_string().contains("public HTTPS address"));
        settings.navidrome_public_url = "https://music.example.test".to_owned();
        settings.covers_listen = "0.0.0.0:4534".to_owned();
        assert!(PluginManager::validate_discord_rpc_config(&settings, true).is_err());
    }

    #[test]
    fn discord_rpc_settings_round_trip_with_user_only_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            plugins_dir: directory.path().join("plugins"),
            ..Config::default()
        };
        let settings = DiscordRpcConfig {
            navidrome_user: "alice".to_owned(),
            navidrome_password: "secret".to_owned(),
            ..DiscordRpcConfig::default()
        };

        PluginManager::save_discord_rpc_config(&config, &settings).unwrap();
        let loaded = PluginManager::load_discord_rpc_config(&config).unwrap();
        assert_eq!(loaded.navidrome_user, "alice");
        assert_eq!(loaded.navidrome_password, "secret");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(PluginManager::discord_rpc_config_path(&config))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn empty_cover_listener_uses_safe_loopback_default() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            plugins_dir: directory.path().join("plugins"),
            ..Config::default()
        };
        let settings = DiscordRpcConfig {
            cover_mode: CoverMode::Server,
            navidrome_public_url: "https://art.example.test".to_owned(),
            covers_listen: String::new(),
            ..DiscordRpcConfig::default()
        };

        PluginManager::save_discord_rpc_config(&config, &settings).unwrap();
        let loaded = PluginManager::load_discord_rpc_config(&config).unwrap();
        assert_eq!(loaded.covers_listen, "127.0.0.1:4534");
    }

    #[test]
    fn discord_rpc_environment_uses_the_upstream_variable_names() {
        let settings = DiscordRpcConfig {
            navidrome_user: "alice".to_owned(),
            navidrome_password: "secret".to_owned(),
            cover_mode: CoverMode::Server,
            navidrome_public_url: "https://music.example.test".to_owned(),
            discord_client_id: "123456789".to_owned(),
            poll_interval: 4,
            ..DiscordRpcConfig::default()
        };
        let environment: std::collections::HashMap<_, _> =
            PluginManager::discord_rpc_environment(&settings)
                .into_iter()
                .collect();
        assert_eq!(environment["NAVIDROME_USER"], "alice");
        assert_eq!(environment["NAVIDROME_PASSWORD"], "secret");
        assert_eq!(environment["COVER_MODE"], "server");
        assert_eq!(
            environment["NAVIDROME_PUBLIC_URL"],
            "https://music.example.test"
        );
        assert_eq!(environment["POLL_INTERVAL"], "4");
        assert_eq!(environment["DISCORD_CLIENT_ID"], "123456789");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn discord_ipc_discovery_skips_stale_socket_and_uses_live_client() {
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let stale_path = directory.path().join("discord-ipc-0");
        let live_path = directory.path().join("discord-ipc-1");
        let stale = UnixListener::bind(&stale_path).unwrap();
        drop(stale);
        let live = UnixListener::bind(&live_path).unwrap();

        assert_eq!(
            find_connectable_discord_socket_in(directory.path()),
            Some(live_path)
        );
        drop(live);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn discord_ipc_appearance_after_start_triggers_reconnect() {
        let socket = (
            std::path::PathBuf::from("/run/user/1000/discord-ipc-0"),
            1,
            42,
        );
        assert!(discord_ipc_socket_changed(None, Some(&socket)));
        assert!(!discord_ipc_socket_changed(None, None));
        assert!(!discord_ipc_socket_changed(Some(&socket), Some(&socket)));
        assert!(!should_restart_rpc_for_discord_socket(
            false,
            None,
            Some(&socket)
        ));
        assert!(should_restart_rpc_for_discord_socket(
            true,
            None,
            Some(&socket)
        ));
    }
}
