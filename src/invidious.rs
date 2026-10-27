use std::fs;
use std::io::{copy, Cursor, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::{sleep, timeout};
use zip::ZipArchive;

use crate::config::{Config, ContainerEngine};
use crate::discovery::Discoverer;

#[derive(Debug, Clone)]
pub struct InvidiousProgress {
    pub percent: f32,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InvidiousAppSettings {
    pub domain: String,
    pub external_port: Option<u16>,
    pub https_only: bool,
    pub statistics_enabled: bool,
    pub registration_enabled: bool,
}

impl Default for InvidiousAppSettings {
    fn default() -> Self {
        Self {
            domain: String::new(),
            external_port: None,
            https_only: false,
            statistics_enabled: false,
            registration_enabled: true,
        }
    }
}

#[derive(Clone)]
pub struct InvidiousManager {
    client: Client,
}

impl Default for InvidiousManager {
    fn default() -> Self {
        Self {
            client: Client::builder()
                .user_agent("syncpulse/0.1.0")
                .timeout(Duration::from_secs(120))
                .build()
                .expect("Failed to build reqwest client"),
        }
    }
}

impl InvidiousManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install_dir(config: &Config) -> PathBuf {
        config.data_dir.join("invidious")
    }

    pub fn compose_path(config: &Config) -> PathBuf {
        Self::install_dir(config).join("docker-compose.yml")
    }

    /// The Invidious Compose file SyncPulse should read and edit: its own
    /// managed deployment when present, otherwise the detected or user-pointed
    /// external deployment.
    pub fn active_compose_path(config: &Config) -> PathBuf {
        let managed = Self::compose_path(config);
        if managed.is_file() {
            return managed;
        }
        if let Some(external) = config.invidious_compose_path.as_ref() {
            if external.is_file() {
                return external.clone();
            }
        }
        managed
    }

    pub fn has_editable_compose(config: &Config) -> bool {
        Self::active_compose_path(config).is_file()
    }

    /// The Compose file SyncPulse should operate, together with the directory
    /// it must run in and its file name, or an error when nothing exists yet.
    fn compose_target(config: &Config) -> Result<(PathBuf, PathBuf, String)> {
        let path = Self::active_compose_path(config);
        if !path.is_file() {
            return Err(anyhow!(
                "Invidious has not been deployed yet; there is no Compose file to operate"
            ));
        }
        let directory = path
            .parent()
            .context("Invidious Compose file has no parent directory")?
            .to_path_buf();
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("Invidious Compose file name is not valid UTF-8")?
            .to_owned();
        Ok((path, directory, file_name))
    }

    /// Finds an Invidious deployment SyncPulse did not create by asking the
    /// container engine which container publishes the configured port and
    /// reading that service's Compose project file from its labels.
    pub async fn detect_external_compose(config: &Config) -> Option<PathBuf> {
        let engine = config.container_engine;
        let ids = capture(
            engine,
            &[
                "ps",
                "--filter",
                &format!("publish={}", config.invidious_port),
                "--format",
                "{{.ID}}",
            ],
        )
        .await?;
        for id in ids.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let image = capture(engine, &["inspect", "--format", "{{.Config.Image}}", id])
                .await
                .unwrap_or_default();
            if !image.to_ascii_lowercase().contains("invidious") {
                continue;
            }
            let files = capture(
                engine,
                &[
                    "inspect",
                    "--format",
                    r#"{{index .Config.Labels "com.docker.compose.project.config_files"}}"#,
                    id,
                ],
            )
            .await
            .unwrap_or_default();
            // A project can list several Compose files, comma-separated.
            for candidate in files.split(',').map(str::trim).filter(|f| !f.is_empty()) {
                let path = PathBuf::from(candidate);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
        None
    }

    /// True only when SyncPulse's own generated deployment exists. A detected
    /// external Compose file remains user-managed and must never be rebound.
    pub fn is_installed(config: &Config) -> bool {
        Self::compose_path(config).is_file()
    }

    pub fn configure_managed_network_access(config: &Config) -> Result<()> {
        let path = Self::compose_path(config);
        if Self::is_installed(config) {
            let address = crate::network::service_bind_address(config)?;
            if let Ok(ip) = address.parse::<std::net::IpAddr>() {
                let probe_ip = if ip.is_unspecified() {
                    "127.0.0.1".parse().expect("IPv4 literal")
                } else {
                    ip
                };
                if std::net::TcpStream::connect_timeout(
                    &std::net::SocketAddr::new(probe_ip, config.invidious_port),
                    Duration::from_millis(150),
                )
                .is_ok()
                {
                    return Err(anyhow!("Port {} is already accepting connections; stop the existing service before updating its Invidious Compose binding", config.invidious_port));
                }
            }
            set_compose_port(&path, config.invidious_port, &address)?;
        }
        Ok(())
    }

    pub async fn install(
        &self,
        config: &Config,
        port: u16,
        progress: &Sender<InvidiousProgress>,
    ) -> Result<()> {
        if Self::is_installed(config) {
            return Err(anyhow!(
                "Invidious is already installed in its managed folder"
            ));
        }
        // Reject a disconnected Tailscale mode before downloading or creating
        // deployment files; never silently fall back to a broader interface.
        let bind_address = crate::network::service_bind_address(config)?;
        // Never install a second deployment on the same port when SyncPulse
        // already detected an Invidious deployment the user set up themselves.
        if Self::has_editable_compose(config) {
            return Err(anyhow!(
                "An Invidious deployment already exists at {}. Start it from the Services page instead of installing a duplicate.",
                Self::active_compose_path(config).display()
            ));
        }
        verify_compose_available(config.container_engine).await?;
        let install_dir = Self::install_dir(config);
        fs::create_dir_all(&install_dir)?;
        send_progress(
            progress,
            5.0,
            "Downloading official Invidious database files",
        );

        let source = self
            .client
            .get("https://github.com/iv-org/invidious/archive/refs/heads/master.zip")
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        if source.is_empty() {
            return Err(anyhow!("Official Invidious source archive is empty"));
        }
        extract_database_files(&source, &install_dir)?;
        send_progress(
            progress,
            20.0,
            "Creating private local deployment configuration",
        );
        let compose = create_compose_file(port, &bind_address)?;
        write_private_file(&Self::compose_path(config), compose.as_bytes())?;

        send_progress(
            progress,
            30.0,
            "Starting Invidious and its database containers",
        );
        compose_action(
            config.container_engine,
            &install_dir,
            "docker-compose.yml",
            Some("syncpulse-invidious"),
            &["up", "-d"],
        )
        .await?;
        send_progress(progress, 70.0, "Waiting for Invidious health check");
        self.wait_until_available(config, port).await?;
        send_progress(progress, 100.0, "Invidious is ready");
        Ok(())
    }

    /// Starts the Invidious deployment SyncPulse can see: its own managed
    /// deployment when present, otherwise the deployment it detected.
    pub async fn start(&self, config: &Config, port: u16) -> Result<()> {
        verify_compose_available(config.container_engine).await?;
        let (path, directory, file_name) = Self::compose_target(config)?;
        let managed = path == Self::compose_path(config);
        if managed {
            let bind_address = crate::network::service_bind_address(config)?;
            let socket_ip = bind_address.parse::<std::net::IpAddr>()?;
            let probe_ip = if socket_ip.is_unspecified() {
                "127.0.0.1".parse().expect("IPv4 literal")
            } else {
                socket_ip
            };
            if std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::new(probe_ip, port),
                Duration::from_millis(150),
            )
            .is_ok()
            {
                return Err(anyhow!("Port {port} is already accepting connections; refusing to start a second Invidious service"));
            }
            set_compose_port(&path, port, &bind_address)?;
        }
        compose_action(
            config.container_engine,
            &directory,
            &file_name,
            managed.then_some("syncpulse-invidious"),
            &["up", "-d"],
        )
        .await?;
        self.wait_until_available(config, port).await
    }

    pub async fn stop(&self, config: &Config) -> Result<()> {
        verify_compose_available(config.container_engine).await?;
        let (path, directory, file_name) = Self::compose_target(config)?;
        let managed = path == Self::compose_path(config);
        compose_action(
            config.container_engine,
            &directory,
            &file_name,
            managed.then_some("syncpulse-invidious"),
            &["stop"],
        )
        .await
    }

    pub fn read_compose(config: &Config) -> Result<String> {
        let path = Self::active_compose_path(config);
        fs::read_to_string(&path).with_context(|| {
            format!(
                "Unable to read Invidious Compose file at {}",
                path.display()
            )
        })
    }

    pub fn compose_app_settings(config: &Config) -> Result<(InvidiousAppSettings, bool)> {
        let contents = Self::read_compose(config)?;
        Ok(parse_app_settings(&contents))
    }

    pub fn save_compose(
        config: &Config,
        contents: &str,
        settings: &InvidiousAppSettings,
    ) -> Result<String> {
        if contents.trim().is_empty() {
            return Err(anyhow!("Invidious Compose configuration cannot be empty"));
        }
        if contents.len() > 1_000_000 {
            return Err(anyhow!("Invidious Compose configuration exceeds 1 MB"));
        }
        validate_app_settings(settings)?;
        let updated = update_app_settings(contents, settings)?;
        let (path, _, _) = Self::compose_target(config)?;
        write_private_file_replace(&path, updated.as_bytes())?;
        Ok(updated)
    }

    pub async fn validate_compose(&self, config: &Config) -> Result<()> {
        verify_compose_available(config.container_engine).await?;
        let (_, directory, file_name) = Self::compose_target(config)?;
        compose_validate(config.container_engine, &directory, &file_name).await
    }

    async fn wait_until_available(&self, config: &Config, port: u16) -> Result<()> {
        let discoverer = Discoverer::new();
        let managed = Self::is_installed(config);
        let host = if managed {
            crate::network::service_probe_address(config).unwrap_or_else(|_| "127.0.0.1".to_owned())
        } else {
            "127.0.0.1".to_owned()
        };
        let result = timeout(Duration::from_secs(180), async {
            loop {
                if discoverer.check_invidious_at(&host, port).await {
                    return Ok(());
                }
                sleep(Duration::from_secs(3)).await;
            }
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => Err(anyhow!(
                "Invidious did not respond on port {port} within 3 minutes; containers may still be starting"
            )),
        }
    }
}

async fn verify_compose_available(engine: ContainerEngine) -> Result<()> {
    let executable = engine_executable(engine);
    let engine_name = engine_name(engine);
    let daemon = Command::new(executable)
        .args(["info"])
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("{engine_name} is required to install or operate Invidious"))?;
    if !daemon.status.success() {
        return Err(anyhow!(
            "{engine_name} is not available. Install and start it, then retry: {}",
            String::from_utf8_lossy(&daemon.stderr).trim()
        ));
    }
    let output = Command::new(executable)
        .args(["compose", "version"])
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("{engine_name} Compose provider is required"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "{engine_name} Compose provider is unavailable: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn engine_executable(engine: ContainerEngine) -> &'static str {
    match engine {
        ContainerEngine::Docker => "docker",
        ContainerEngine::Podman => "podman",
    }
}

fn engine_name(engine: ContainerEngine) -> &'static str {
    match engine {
        ContainerEngine::Docker => "Docker Engine",
        ContainerEngine::Podman => "Podman",
    }
}

async fn compose_action(
    engine: ContainerEngine,
    directory: &Path,
    file_name: &str,
    project: Option<&str>,
    arguments: &[&str],
) -> Result<()> {
    let mut command = Command::new(engine_executable(engine));
    command.args(["compose", "-f", file_name]);
    // A detected deployment keeps its own Compose project name, which Compose
    // derives from the directory; forcing SyncPulse's name would create a
    // second stack that fights over the same ports.
    if let Some(project) = project {
        command.args(["-p", project]);
    }
    command
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = timeout(Duration::from_secs(20 * 60), command.output())
        .await
        .context("Container Compose operation timed out after 20 minutes")?
        .context("Unable to run the selected Compose provider")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow!(
            "{} Compose failed: {}{}",
            engine_name(engine),
            stderr.trim(),
            if stdout.trim().is_empty() {
                String::new()
            } else {
                format!(" {}", stdout.trim())
            }
        ));
    }
    Ok(())
}

/// Runs a bare container-engine command and returns its stdout, or `None` when
/// the engine is missing or the command fails. Used for read-only detection.
async fn capture(engine: ContainerEngine, arguments: &[&str]) -> Option<String> {
    let output = Command::new(engine_executable(engine))
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Validates a specific Compose file with the selected engine, without assuming
/// it lives in SyncPulse's managed folder.
async fn compose_validate(
    engine: ContainerEngine,
    directory: &Path,
    file_name: &str,
) -> Result<()> {
    let mut command = Command::new(engine_executable(engine));
    command
        .args(["compose", "-f", file_name, "config", "--quiet"])
        .current_dir(directory)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = timeout(Duration::from_secs(60), command.output())
        .await
        .context("Container Compose validation timed out")?
        .context("Unable to run the selected Compose provider")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "{} Compose validation failed: {}",
            engine_name(engine),
            stderr.trim()
        ));
    }
    Ok(())
}

fn set_compose_port(path: &Path, port: u16, bind_address: &str) -> Result<()> {
    let current =
        fs::read_to_string(path).with_context(|| format!("Unable to read {}", path.display()))?;
    let pattern = regex::Regex::new(
        r#"(?m)^      - "(?:(?:127\.0\.0\.1|0\.0\.0\.0|\d{1,3}(?:\.\d{1,3}){3}):)?(\d+):3000"$"#,
    )
    .expect("valid compose port pattern");
    let replacement = if bind_address == "0.0.0.0" {
        format!(r#"      - "{port}:3000""#)
    } else {
        format!(r#"      - "{bind_address}:{port}:3000""#)
    };
    let updated = pattern.replace_all(&current, replacement.as_str());
    if updated == current {
        let already_matches = if bind_address == "0.0.0.0" {
            regex::Regex::new(&format!(r#"(?m)^      - "(?:{port}:3000|(?:127\.0\.0\.1|0\.0\.0\.0|\d{{1,3}}(?:\.\d{{1,3}}){{3}}):{port}:3000)"$"#))
        } else {
            regex::Regex::new(&format!(r#"(?m)^      - "{bind_address}:{port}:3000"$"#))
        }
        .expect("valid port pattern")
        .is_match(&current);
        if already_matches {
            return Ok(());
        }
        return Err(anyhow!("Invidious Compose file does not contain the managed localhost port mapping. Edit its ports section to match the configured port, or restore the standard mapping."));
    }
    let mut temporary = tempfile::NamedTempFile::new_in(
        path.parent()
            .context("Invidious compose file has no parent directory")?,
    )?;
    temporary.write_all(updated.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .with_context(|| format!("Unable to update {}", path.display()))?;
    Ok(())
}

fn parse_app_settings(contents: &str) -> (InvidiousAppSettings, bool) {
    let lines: Vec<_> = contents.lines().collect();
    let Some((start, end, indent)) = invidious_config_block(&lines) else {
        return (InvidiousAppSettings::default(), false);
    };
    let mut settings = InvidiousAppSettings::default();
    for line in &lines[start + 1..end] {
        let content = line.strip_prefix(&" ".repeat(indent)).unwrap_or_default();
        let Some((key, value)) = content.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        match key.trim() {
            "domain" => settings.domain = value.to_owned(),
            "external_port" => settings.external_port = value.parse().ok(),
            "https_only" => settings.https_only = value.eq_ignore_ascii_case("true"),
            "statistics_enabled" => {
                settings.statistics_enabled = value.eq_ignore_ascii_case("true")
            }
            "registration_enabled" => {
                settings.registration_enabled = value.eq_ignore_ascii_case("true")
            }
            _ => {}
        }
    }
    (settings, true)
}

fn update_app_settings(contents: &str, settings: &InvidiousAppSettings) -> Result<String> {
    let mut lines: Vec<String> = contents.lines().map(str::to_owned).collect();
    let Some((start, end, indent)) =
        invidious_config_block(&lines.iter().map(String::as_str).collect::<Vec<_>>())
    else {
        return Ok(contents.to_owned());
    };
    let entries = [
        (
            "domain",
            if settings.domain.is_empty() {
                String::new()
            } else {
                serde_json::to_string(settings.domain.trim())?
            },
        ),
        (
            "external_port",
            settings
                .external_port
                .map(|port| port.to_string())
                .unwrap_or_default(),
        ),
        ("https_only", settings.https_only.to_string()),
        (
            "statistics_enabled",
            settings.statistics_enabled.to_string(),
        ),
        (
            "registration_enabled",
            settings.registration_enabled.to_string(),
        ),
    ];
    let prefix = " ".repeat(indent);
    let mut missing = Vec::new();
    for (key, value) in entries {
        let line_prefix = format!("{prefix}{key}:");
        if let Some(line) = lines[start + 1..end]
            .iter_mut()
            .find(|line| line.starts_with(&line_prefix))
        {
            *line = format!("{line_prefix} {value}");
        } else {
            missing.push(format!("{line_prefix} {value}"));
        }
    }
    lines.splice(end..end, missing);
    let mut updated = lines.join("\n");
    if contents.ends_with('\n') && !updated.ends_with('\n') {
        updated.push('\n');
    }
    Ok(updated)
}

fn invidious_config_block(lines: &[&str]) -> Option<(usize, usize, usize)> {
    let (start, config_line) = lines.iter().enumerate().find(|(_, line)| {
        let trimmed = line.trim_start();
        trimmed.starts_with("INVIDIOUS_CONFIG: |")
    })?;
    let parent_indent = config_line.len() - config_line.trim_start().len();
    let root_indent = parent_indent + 2;
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| {
            !line.trim().is_empty() && line.len() - line.trim_start().len() <= parent_indent
        })
        .map(|(index, _)| index)
        .unwrap_or(lines.len());
    Some((start, end, root_indent))
}

fn validate_app_settings(settings: &InvidiousAppSettings) -> Result<()> {
    let domain = settings.domain.trim();
    if settings.external_port == Some(0) {
        return Err(anyhow!("External proxy port must be between 1 and 65535"));
    }
    if !domain.is_empty()
        && (domain.contains("://")
            || domain.contains('/')
            || domain.chars().any(char::is_whitespace)
            || url::Url::parse(&format!("https://{domain}"))
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_none_or(|hostname| !hostname.eq_ignore_ascii_case(domain)))
    {
        return Err(anyhow!(
            "Invidious domain must be a hostname only, without a scheme or path"
        ));
    }
    Ok(())
}

fn extract_database_files(source: &[u8], install_dir: &Path) -> Result<()> {
    let mut archive = ZipArchive::new(Cursor::new(source))
        .context("Downloaded Invidious source archive is invalid")?;
    let mut sql_count = 0;
    let mut init_script_found = false;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if !entry.is_file() {
            continue;
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            continue;
        }
        let name = entry.name().replace('\\', "/");
        let Some((_, relative)) = name.split_once('/') else {
            continue;
        };
        let relative_path = Path::new(relative);
        if !safe_relative_path(relative_path) {
            continue;
        }
        let is_sql = relative.starts_with("config/sql/") && relative.ends_with(".sql");
        let is_init = relative == "docker/init-invidious-db.sh";
        if !is_sql && !is_init {
            continue;
        }
        let destination = install_dir.join(relative_path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = fs::File::create(&destination)?;
        copy(&mut entry, &mut output)?;
        output.sync_all()?;
        sql_count += usize::from(is_sql);
        init_script_found |= is_init;
    }
    if sql_count == 0 || !init_script_found {
        return Err(anyhow!(
            "Official Invidious archive is missing required database initialization files"
        ));
    }
    Ok(())
}

fn safe_relative_path(path: &Path) -> bool {
    path.components()
        .all(|component| matches!(component, Component::Normal(_)))
}

fn create_compose_file(port: u16, bind_address: &str) -> Result<String> {
    let hmac_key = random_secret()?;
    let companion_key = random_secret()?;
    let database_password = random_secret()?;
    let port_mapping = if bind_address == "0.0.0.0" {
        format!("{port}:3000")
    } else {
        format!("{bind_address}:{port}:3000")
    };
    // Docker copies the host's /etc/resolv.conf into each container when it is
    // created. If the host's DNS later changes (VPN toggled, NetworkManager
    // rewrites resolv.conf), a container that is never recreated keeps the now
    // dead upstream servers, fails to resolve youtube.com and returns
    // "500 Internal Server Error" as plain text. Invidious then fails to parse
    // it and reports "Unexpected char 'I' at line 1, column 1". Pinning the
    // upstream resolvers makes the deployment independent of host churn; the
    // embedded 127.0.0.11 resolver still handles service-name lookups.
    Ok(format!(
        r#"services:
  invidious:
    image: quay.io/invidious/invidious:latest
    restart: unless-stopped
    dns:
      - 1.1.1.1
      - 9.9.9.9
    ports:
      - "{port_mapping}"
    environment:
      INVIDIOUS_CONFIG: |
        db:
          dbname: invidious
          user: kemal
          password: "{database_password}"
          host: invidious-db
          port: 5432
        check_tables: true
        invidious_companion:
          - private_url: "http://companion:8282/companion"
        invidious_companion_key: "{companion_key}"
        hmac_key: "{hmac_key}"
    depends_on:
      invidious-db:
        condition: service_healthy
      companion:
        condition: service_started
    healthcheck:
      test: ["CMD-SHELL", "wget -nv --tries=1 --spider http://127.0.0.1:3000/api/v1/stats || exit 1"]
      interval: 30s
      timeout: 5s
      retries: 3
    logging:
      options:
        max-size: "1G"
        max-file: "4"

  companion:
    image: quay.io/invidious/invidious-companion:latest
    environment:
      SERVER_SECRET_KEY: "{companion_key}"
    restart: unless-stopped
    dns:
      - 1.1.1.1
      - 9.9.9.9
    cap_drop:
      - ALL
    read_only: true
    volumes:
      - companioncache:/var/tmp/youtubei.js:rw
    security_opt:
      - no-new-privileges:true
    logging:
      options:
        max-size: "1G"
        max-file: "4"

  invidious-db:
    image: docker.io/library/postgres:14
    restart: unless-stopped
    volumes:
      - postgresdata:/var/lib/postgresql/data
      - ./config/sql:/config/sql:ro
      - ./docker/init-invidious-db.sh:/docker-entrypoint-initdb.d/init-invidious-db.sh:ro
    environment:
      POSTGRES_DB: invidious
      POSTGRES_USER: kemal
      POSTGRES_PASSWORD: "{database_password}"
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U $$POSTGRES_USER -d $$POSTGRES_DB"]
      interval: 10s
      timeout: 5s
      retries: 5

volumes:
  postgresdata:
  companioncache:
"#
    ))
}

fn random_secret() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).context("Unable to generate a secure Invidious secret")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("Invidious configuration path has no parent directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)
        .with_context(|| format!("Unable to create {}", path.display()))?;
    Ok(())
}

fn write_private_file_replace(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("Invidious configuration path has no parent directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .with_context(|| format!("Unable to update {}", path.display()))?;
    Ok(())
}

fn send_progress(progress: &Sender<InvidiousProgress>, percent: f32, message: &str) {
    let _ = progress.send(InvidiousProgress {
        percent,
        message: message.to_owned(),
    });
}

#[cfg(test)]
mod tests {
    use super::{
        create_compose_file, engine_executable, parse_app_settings, safe_relative_path,
        set_compose_port, update_app_settings, InvidiousAppSettings, InvidiousManager,
    };
    use crate::config::{Config, ContainerEngine};
    use std::fs;
    use std::path::Path;

    #[test]
    fn compose_file_binds_only_to_localhost_and_uses_distinct_secrets() {
        let compose =
            create_compose_file(3000, "127.0.0.1").expect("compose config should generate");
        assert!(compose.contains("\"127.0.0.1:3000:3000\""));
        assert!(!compose.contains("CHANGE_ME"));
        assert_eq!(compose.matches("SERVER_SECRET_KEY: \"").count(), 1);
        assert!(compose.contains("init-invidious-db.sh:ro"));
        // Invidious and its companion must pin upstream DNS so the deployment
        // survives a host resolv.conf change instead of returning plain-text
        // "500 Internal Server Error" that Invidious cannot parse as JSON.
        assert_eq!(compose.matches("\n    dns:\n").count(), 2);
        assert!(compose.contains("      - 1.1.1.1\n"));
    }

    #[test]
    fn compose_file_can_explicitly_bind_for_other_devices() {
        let compose = create_compose_file(3000, "0.0.0.0").expect("compose config should generate");
        assert!(compose.contains("\"3000:3000\""));
        assert!(!compose.contains("127.0.0.1:3000:3000"));
    }

    #[test]
    fn generated_compose_configuration_is_valid_when_docker_compose_is_available() {
        use std::fs;
        use std::process::Command;

        let directory = tempfile::tempdir().expect("temporary deployment directory");
        let compose_path = directory.path().join("docker-compose.yml");
        fs::create_dir_all(directory.path().join("config/sql")).expect("SQL directory");
        fs::create_dir_all(directory.path().join("docker")).expect("Docker directory");
        fs::write(
            directory.path().join("docker/init-invidious-db.sh"),
            "#!/bin/sh\n",
        )
        .expect("database initializer");
        fs::write(
            &compose_path,
            create_compose_file(3000, "127.0.0.1").expect("compose config should generate"),
        )
        .expect("compose file");

        let output = match Command::new("docker")
            .args(["compose", "-f"])
            .arg(&compose_path)
            .args(["config", "--quiet"])
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("Could not validate docker compose file: {error}"),
        };
        assert!(
            output.status.success(),
            "docker compose rejected generated config: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn source_archive_paths_cannot_escape_the_deployment_directory() {
        assert!(safe_relative_path(Path::new("config/sql/init.sql")));
        assert!(!safe_relative_path(Path::new("../outside")));
        assert!(!safe_relative_path(Path::new("/absolute/path")));
    }

    #[test]
    fn changing_service_port_updates_only_the_managed_local_binding() {
        let directory = tempfile::tempdir().expect("temporary deployment directory");
        let compose_path = directory.path().join("docker-compose.yml");
        fs::write(
            &compose_path,
            "    ports:\n      - \"127.0.0.1:3000:3000\"\n",
        )
        .expect("initial compose file");

        set_compose_port(&compose_path, 3456, "127.0.0.1").expect("port mapping should update");

        let compose = fs::read_to_string(compose_path).expect("updated compose file");
        assert!(compose.contains("\"127.0.0.1:3456:3000\""));
        assert!(!compose.contains("\"127.0.0.1:3000:3000\""));
    }

    #[test]
    fn service_binding_change_replaces_a_prior_wildcard_host_mapping() {
        let directory = tempfile::tempdir().expect("temporary deployment directory");
        let compose_path = directory.path().join("docker-compose.yml");
        fs::write(&compose_path, "    ports:\n      - \"3000:3000\"\n")
            .expect("initial wildcard compose file");

        set_compose_port(&compose_path, 3000, "127.0.0.1")
            .expect("restore private localhost mapping");

        let compose = fs::read_to_string(compose_path).expect("updated compose file");
        assert!(compose.contains("\"127.0.0.1:3000:3000\""));
        assert!(!compose.contains("\"3000:3000\""));
    }

    #[test]
    fn network_access_switch_updates_managed_invidious_binding() {
        let directory = tempfile::tempdir().expect("temporary deployment directory");
        let compose_path = directory.path().join("docker-compose.yml");
        fs::write(
            &compose_path,
            "    ports:\n      - \"127.0.0.1:3000:3000\"\n",
        )
        .expect("initial compose file");

        set_compose_port(&compose_path, 3000, "0.0.0.0").expect("enable LAN mapping");
        let lan = fs::read_to_string(&compose_path).expect("LAN compose file");
        assert!(lan.contains("\"3000:3000\""));
        assert!(!lan.contains("127.0.0.1"));

        set_compose_port(&compose_path, 3000, "127.0.0.1").expect("restore loopback mapping");
        let local = fs::read_to_string(compose_path).expect("local compose file");
        assert!(local.contains("\"127.0.0.1:3000:3000\""));
    }

    #[test]
    fn tailscale_binding_is_written_as_a_specific_private_address() {
        let directory = tempfile::tempdir().expect("temporary deployment directory");
        let compose_path = directory.path().join("docker-compose.yml");
        fs::write(
            &compose_path,
            "    ports:\n      - \"127.0.0.1:3000:3000\"\n",
        )
        .expect("initial compose file");
        set_compose_port(&compose_path, 3000, "100.64.1.20").expect("set Tailscale binding");
        let compose = fs::read_to_string(compose_path).expect("updated compose file");
        assert!(compose.contains("\"100.64.1.20:3000:3000\""));
        assert!(!compose.contains("0.0.0.0"));
    }

    #[test]
    fn container_engine_selects_its_own_compose_executable() {
        assert_eq!(engine_executable(ContainerEngine::Docker), "docker");
        assert_eq!(engine_executable(ContainerEngine::Podman), "podman");
    }

    #[test]
    fn compose_editor_saves_the_managed_file_with_private_permissions() {
        let directory = tempfile::tempdir().expect("temporary data directory");
        let config = Config {
            data_dir: directory.path().to_path_buf(),
            ..Config::default()
        };
        let compose_path = InvidiousManager::compose_path(&config);
        fs::create_dir_all(compose_path.parent().unwrap()).expect("deployment directory");
        fs::write(&compose_path, "services: {}\n").expect("initial compose");

        InvidiousManager::save_compose(
            &config,
            "services:\n  invidious: {}\n",
            &InvidiousAppSettings::default(),
        )
        .expect("compose editor saves");
        assert_eq!(
            InvidiousManager::read_compose(&config).unwrap(),
            "services:\n  invidious: {}\n"
        );
        assert!(
            InvidiousManager::save_compose(&config, " \n", &InvidiousAppSettings::default())
                .is_err()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(compose_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn app_settings_update_only_known_yaml_keys_and_preserve_generated_secrets() {
        let original = create_compose_file(3000, "127.0.0.1").unwrap();
        let secret = original
            .lines()
            .find(|line| line.contains("hmac_key:"))
            .unwrap()
            .to_owned();
        let settings = InvidiousAppSettings {
            domain: "videos.example.test".to_owned(),
            external_port: Some(8443),
            https_only: true,
            statistics_enabled: false,
            registration_enabled: false,
        };
        let updated = update_app_settings(&original, &settings).unwrap();
        assert!(updated.contains("domain: \"videos.example.test\""));
        assert!(updated.contains("external_port: 8443"));
        assert!(updated.contains("https_only: true"));
        assert!(updated.contains("registration_enabled: false"));
        assert!(updated.contains(&secret));
        assert_eq!(parse_app_settings(&updated), (settings, true));
    }

    #[test]
    fn app_settings_leave_custom_compose_without_inline_config_untouched() {
        let compose = "services:\n  invidious:\n    image: example/invidious\n";
        let settings = InvidiousAppSettings {
            domain: "videos.example.test".to_owned(),
            ..InvidiousAppSettings::default()
        };
        assert_eq!(update_app_settings(compose, &settings).unwrap(), compose);
        assert_eq!(
            parse_app_settings(compose),
            (InvidiousAppSettings::default(), false)
        );
    }

    #[test]
    fn detected_external_deployment_is_read_and_edited_instead_of_the_missing_managed_file() {
        let managed_root = tempfile::tempdir().expect("managed data directory");
        let external_root = tempfile::tempdir().expect("external deployment directory");
        let external = external_root.path().join("docker-compose.yml");
        fs::write(&external, "services: {}\n").expect("external compose");
        let config = Config {
            data_dir: managed_root.path().to_path_buf(),
            invidious_compose_path: Some(external.clone()),
            ..Config::default()
        };

        assert!(!InvidiousManager::is_installed(&config));
        assert!(InvidiousManager::has_editable_compose(&config));
        assert_eq!(InvidiousManager::active_compose_path(&config), external);
        assert_eq!(
            InvidiousManager::read_compose(&config).unwrap(),
            "services: {}\n"
        );
    }

    #[test]
    fn managed_compose_takes_precedence_over_a_detected_external_file() {
        let data_root = tempfile::tempdir().expect("data directory");
        let external_root = tempfile::tempdir().expect("external deployment directory");
        let external = external_root.path().join("docker-compose.yml");
        fs::write(&external, "services: {}\n").expect("external compose");
        let config = Config {
            data_dir: data_root.path().to_path_buf(),
            invidious_compose_path: Some(external),
            ..Config::default()
        };
        let managed = InvidiousManager::compose_path(&config);
        fs::create_dir_all(managed.parent().unwrap()).expect("managed directory");
        fs::write(&managed, "services:\n  invidious: {}\n").expect("managed compose");

        assert!(InvidiousManager::is_installed(&config));
        assert_eq!(InvidiousManager::active_compose_path(&config), managed);
    }

    #[test]
    fn a_detected_path_that_no_longer_exists_falls_back_to_the_managed_location() {
        let data_root = tempfile::tempdir().expect("data directory");
        let external_root = tempfile::tempdir().expect("external deployment directory");
        let config = Config {
            data_dir: data_root.path().to_path_buf(),
            invidious_compose_path: Some(external_root.path().join("removed.yml")),
            ..Config::default()
        };

        assert!(!InvidiousManager::has_editable_compose(&config));
        assert_eq!(
            InvidiousManager::active_compose_path(&config),
            InvidiousManager::compose_path(&config)
        );
    }
}
