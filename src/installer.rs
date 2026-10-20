use std::fs::{self, File};
use std::io::{copy, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tar::Archive;
use tokio::process::Command;
use tokio::time::timeout;
use zip::ZipArchive;

use crate::config::Config;
use crate::utils::{detect_os_arch, is_windows, spawn_detached_no_window, OsArch};

#[derive(Debug, Clone)]
pub struct InstallProgress {
    pub percent: f32,
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize, Clone)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Clone)]
pub struct Installer {
    client: Client,
}

impl Default for Installer {
    fn default() -> Self {
        Self {
            client: Client::builder()
                .user_agent("syncpulse/0.1.0")
                .timeout(Duration::from_secs(300))
                .build()
                .expect("Failed to build reqwest client"),
        }
    }
}

impl Installer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn navidrome_bin_name() -> &'static str {
        if is_windows() {
            "navidrome.exe"
        } else {
            "navidrome"
        }
    }

    pub fn navidrome_bin_path(config: &Config) -> PathBuf {
        config.bin_dir.join(Self::navidrome_bin_name())
    }

    pub fn is_navidrome_installed(config: &Config) -> bool {
        Self::navidrome_bin_path(config).is_file()
    }

    pub async fn install_navidrome(
        &self,
        config: &Config,
        progress: Sender<InstallProgress>,
    ) -> Result<PathBuf> {
        send_progress(&progress, 0.0, "Detecting platform");
        let platform = detect_os_arch();
        if platform == OsArch::Unknown {
            return Err(anyhow!(
                "Navidrome installation is not supported on this platform"
            ));
        }
        fs::create_dir_all(&config.bin_dir)?;
        fs::create_dir_all(&config.data_dir)?;
        fs::create_dir_all(&config.music_dir)?;

        send_progress(&progress, 5.0, "Fetching latest Navidrome release");
        let release: GhRelease = self
            .client
            .get("https://api.github.com/repos/navidrome/navidrome/releases/latest")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        send_progress(&progress, 10.0, &format!("Release {}", release.tag_name));
        let asset = find_asset(&release.assets, platform)
            .ok_or_else(|| anyhow!("No Navidrome release asset for {platform}"))?;
        let archive_path = tempfile::Builder::new()
            .prefix("syncpulse-navidrome-")
            .tempfile()?;
        let archive_path = archive_path.into_temp_path();
        download(&self.client, &asset, &archive_path, &progress).await?;

        send_progress(&progress, 80.0, "Extracting Navidrome binary");
        let binary_path = Self::navidrome_bin_path(config);
        extract_binary(&archive_path, &binary_path, platform)?;
        send_progress(&progress, 90.0, "Verifying Navidrome binary");
        if let Err(error) = verify_binary(&binary_path).await {
            let _ = fs::remove_file(&binary_path);
            return Err(error)
                .context("Downloaded Navidrome binary did not pass its version check");
        }
        write_config(config)?;
        send_progress(&progress, 100.0, "Navidrome installed");
        Ok(binary_path)
    }

    pub fn start_navidrome(&self, config: &Config) -> Result<()> {
        let binary = Self::navidrome_bin_path(config);
        if !binary.is_file() {
            return Err(anyhow!(
                "Navidrome binary not found at {}",
                binary.display()
            ));
        }
        let bind_ip = crate::network::service_bind_address(config)?
            .parse::<std::net::IpAddr>()
            .context("Invalid Navidrome bind address")?;
        let address = if bind_ip.is_unspecified() {
            // A wildcard listener also occupies all interfaces, so detecting
            // only loopback would miss a port collision.
            SocketAddr::new(
                "127.0.0.1".parse().expect("IPv4 literal"),
                config.navidrome_port,
            )
        } else {
            SocketAddr::new(bind_ip, config.navidrome_port)
        };
        if TcpStream::connect_timeout(&address, Duration::from_millis(150)).is_ok() {
            return Err(anyhow!(
                "Port {} is already accepting connections; refusing to start a second Navidrome instance",
                config.navidrome_port
            ));
        }
        write_config(config)?;
        let binary = binary.canonicalize().with_context(|| {
            format!("Unable to resolve Navidrome binary at {}", binary.display())
        })?;
        let working_dir = config
            .bin_dir
            .canonicalize()
            .with_context(|| format!("Unable to resolve {}", config.bin_dir.display()))?;
        let command = binary
            .to_str()
            .ok_or_else(|| anyhow!("Navidrome binary path is not valid UTF-8"))?;
        spawn_detached_no_window(command, &[], Some(&working_dir))
            .with_context(|| format!("Failed to start Navidrome from {}", binary.display()))
    }
}

async fn verify_binary(binary: &Path) -> Result<()> {
    let output = timeout(
        Duration::from_secs(15),
        Command::new(binary).arg("--version").output(),
    )
    .await
    .context("Navidrome version check timed out")?
    .with_context(|| format!("Unable to execute Navidrome binary at {}", binary.display()))?;
    let details = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        return Err(anyhow!(
            "Navidrome --version exited with {}: {}",
            output.status,
            details.trim()
        ));
    }
    if details.trim().is_empty() {
        return Err(anyhow!(
            "Navidrome --version succeeded without returning version information"
        ));
    }
    Ok(())
}

fn find_asset(assets: &[GhAsset], platform: OsArch) -> Option<GhAsset> {
    assets
        .iter()
        .find(|asset| {
            let name = asset.name.to_ascii_lowercase();
            match platform {
                OsArch::LinuxX86_64 => {
                    name.contains("linux")
                        && (name.contains("amd64") || name.contains("x86_64"))
                        && name.ends_with(".tar.gz")
                }
                OsArch::WindowsX86_64 => {
                    name.contains("windows")
                        && (name.contains("amd64") || name.contains("x86_64"))
                        && name.ends_with(".zip")
                }
                OsArch::LinuxAArch64 => {
                    name.contains("linux")
                        && (name.contains("arm64") || name.contains("aarch64"))
                        && name.ends_with(".tar.gz")
                }
                OsArch::WindowsAArch64 => {
                    name.contains("windows")
                        && (name.contains("arm64") || name.contains("aarch64"))
                        && name.ends_with(".zip")
                }
                OsArch::Unknown => false,
            }
        })
        .cloned()
}

async fn download(
    client: &Client,
    asset: &GhAsset,
    destination: &Path,
    progress: &Sender<InstallProgress>,
) -> Result<()> {
    let mut response = client
        .get(&asset.browser_download_url)
        .send()
        .await?
        .error_for_status()?;
    let mut file = File::create(destination)?;
    let mut downloaded = 0_u64;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk)?;
        downloaded += chunk.len() as u64;
        let percent = if asset.size == 0 {
            15.0
        } else {
            15.0 + 60.0 * (downloaded as f32 / asset.size as f32).min(1.0)
        };
        send_progress(progress, percent, "Downloading Navidrome");
    }
    file.sync_all()?;
    if downloaded == 0 {
        return Err(anyhow!("Downloaded Navidrome archive is empty"));
    }
    Ok(())
}

fn extract_binary(archive_path: &Path, destination: &Path, platform: OsArch) -> Result<()> {
    let expected_name = match platform {
        OsArch::WindowsX86_64 | OsArch::WindowsAArch64 => "navidrome.exe",
        OsArch::LinuxX86_64 | OsArch::LinuxAArch64 => "navidrome",
        OsArch::Unknown => return Err(anyhow!("Unsupported platform")),
    };
    let parent = destination
        .parent()
        .context("Navidrome binary path has no parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let mut found = false;

    match platform {
        OsArch::LinuxX86_64 | OsArch::LinuxAArch64 => {
            let decoder = GzDecoder::new(File::open(archive_path)?);
            let mut archive = Archive::new(decoder);
            for entry in archive.entries()? {
                let mut entry = entry?;
                if !entry.header().entry_type().is_file()
                    || entry.path()?.file_name().and_then(|name| name.to_str())
                        != Some(expected_name)
                {
                    continue;
                }
                copy(&mut entry, temporary.as_file_mut())?;
                found = true;
                break;
            }
        }
        OsArch::WindowsX86_64 | OsArch::WindowsAArch64 => {
            let mut archive = ZipArchive::new(File::open(archive_path)?)?;
            for index in 0..archive.len() {
                let mut entry = archive.by_index(index)?;
                if !entry.is_file()
                    || Path::new(entry.name())
                        .file_name()
                        .and_then(|name| name.to_str())
                        != Some(expected_name)
                {
                    continue;
                }
                copy(&mut entry, temporary.as_file_mut())?;
                found = true;
                break;
            }
        }
        OsArch::Unknown => return Err(anyhow!("Unsupported platform")),
    }
    if !found {
        return Err(anyhow!(
            "{expected_name} not found in Navidrome release archive"
        ));
    }

    if temporary.as_file().metadata()?.len() == 0 {
        return Err(anyhow!("Extracted Navidrome binary is empty"));
    }
    temporary.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    temporary
        .persist(destination)
        .with_context(|| format!("Unable to install binary at {}", destination.display()))?;
    Ok(())
}

#[derive(Serialize)]
struct NavidromeConfig {
    #[serde(rename = "MusicFolder")]
    music_folder: String,
    #[serde(rename = "DataFolder")]
    data_folder: String,
    #[serde(rename = "Port")]
    port: u16,
    #[serde(rename = "Address")]
    address: String,
}

pub(crate) fn write_config(config: &Config) -> Result<()> {
    let path = config.bin_dir.join("navidrome.toml");
    let navidrome_config = NavidromeConfig {
        music_folder: fs::canonicalize(&config.music_dir)
            .with_context(|| format!("Unable to resolve {}", config.music_dir.display()))?
            .to_string_lossy()
            .into_owned(),
        data_folder: fs::canonicalize(&config.data_dir)
            .with_context(|| format!("Unable to resolve {}", config.data_dir.display()))?
            .to_string_lossy()
            .into_owned(),
        port: config.navidrome_port,
        address: crate::network::service_bind_address(config)?,
    };
    let content = toml::to_string_pretty(&navidrome_config)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&config.bin_dir)?;
    temporary.write_all(content.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&path)
        .with_context(|| format!("Unable to write Navidrome config at {}", path.display()))?;
    Ok(())
}

fn send_progress(tx: &Sender<InstallProgress>, percent: f32, message: &str) {
    let _ = tx.send(InstallProgress {
        percent,
        message: message.to_owned(),
    });
}

#[cfg(test)]
mod tests {
    use super::{find_asset, verify_binary, write_config, GhAsset};
    use crate::config::Config;
    use crate::utils::OsArch;
    use std::fs;

    fn asset(name: &str) -> GhAsset {
        GhAsset {
            name: name.to_owned(),
            browser_download_url: String::new(),
            size: 0,
        }
    }

    #[test]
    fn release_asset_selection_matches_platform_and_archive_type() {
        let assets = vec![
            asset("navidrome_1_linux_amd64.tar.gz"),
            asset("navidrome_1_windows_amd64.zip"),
            asset("navidrome_1_darwin_arm64.tar.gz"),
        ];
        assert_eq!(
            find_asset(&assets, OsArch::LinuxX86_64).map(|asset| asset.name),
            Some("navidrome_1_linux_amd64.tar.gz".to_owned())
        );
        assert_eq!(
            find_asset(&assets, OsArch::WindowsX86_64).map(|asset| asset.name),
            Some("navidrome_1_windows_amd64.zip".to_owned())
        );
        assert!(find_asset(&assets, OsArch::LinuxAArch64).is_none());
    }

    #[test]
    fn generated_config_uses_resolved_music_and_data_paths() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            bin_dir: temp.path().join("bin"),
            music_dir: temp.path().join("music"),
            data_dir: temp.path().join("data"),
            ..Config::default()
        };
        fs::create_dir_all(&config.bin_dir).unwrap();
        fs::create_dir_all(&config.music_dir).unwrap();
        fs::create_dir_all(&config.data_dir).unwrap();

        write_config(&config).unwrap();

        let content = fs::read_to_string(config.bin_dir.join("navidrome.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(
            parsed["MusicFolder"].as_str(),
            Some(config.music_dir.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(
            parsed["DataFolder"].as_str(),
            Some(config.data_dir.canonicalize().unwrap().to_str().unwrap())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binary_verification_requires_a_successful_version_command() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let valid = directory.path().join("navidrome-valid");
        fs::write(&valid, "#!/bin/sh\nprintf 'navidrome version test\\n'\n").unwrap();
        fs::set_permissions(&valid, fs::Permissions::from_mode(0o755)).unwrap();
        verify_binary(&valid).await.unwrap();

        let invalid = directory.path().join("navidrome-invalid");
        fs::write(&invalid, "#!/bin/sh\nexit 2\n").unwrap();
        fs::set_permissions(&invalid, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_binary(&invalid).await.is_err());
    }
}
