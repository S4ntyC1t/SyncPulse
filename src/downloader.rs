use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{copy, Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use md5::{Digest, Md5};
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use tempfile::{Builder as TempFileBuilder, TempDir};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;
use zip::ZipArchive;

use crate::config::Config;
use crate::utils::{detect_os_arch, is_windows, OsArch};

/// Removes the clutter that uploaders add to video titles, such as
/// `Song [Official Video] (4K) | Album`, leaving the song name.
const TITLE_CLEANUP_PATTERN: &str = r"\[[^\]]*\]|【[^】]*】|\([^)]*\)|（[^）]*）|\s*[|｜].*$";

const AUDIO_EXTRACTION_ARGS: &[&str] = &[
    "--yes-playlist",
    "--extract-audio",
    "--embed-metadata",
    "--embed-thumbnail",
    // YouTube does not consistently expose the song's artist separately.
    // Prefer the channel/uploader attribution over generic fallback values
    // such as "YouTube Downloader" so Navidrome shows the credited creator.
    "--parse-metadata",
    "%(uploader,channel,creator)s:artist",
    "--parse-metadata",
    "%(uploader,channel,creator)s:album_artist",
    // Keep real playlists together under their playlist title. For a single
    // video, use that track's title as the album instead of yt-dlp's generic
    // "YouTube Downloader" album, which makes unrelated songs appear together.
    "--parse-metadata",
    "%(playlist_title,title)s:album",
    // Keep going when a single item is unavailable; without this a playlist
    // stops at the first bad video and everything downloaded so far is lost.
    "--ignore-errors",
    // Recent yt-dlp releases need a JavaScript runtime for some YouTube
    // players. Node is enabled when it is installed and ignored otherwise.
    "--js-runtimes",
    "node",
    // Strip the title clutter before it is embedded and used for file names.
    "--replace-in-metadata",
    "title",
    TITLE_CLEANUP_PATTERN,
    "",
    "--replace-in-metadata",
    "title",
    r"\s+$",
    "",
    "--replace-in-metadata",
    "title",
    r"\s{2,}",
    " ",
    "--no-warnings",
    "--newline",
    "--progress",
];

#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub percent: f32,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub url: String,
    pub format: AudioFormat,
    pub quality: AudioQuality,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Mp3,
    Flac,
    M4a,
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self::Mp3
    }
}

impl std::fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mp3 => write!(f, "mp3"),
            Self::Flac => write!(f, "flac"),
            Self::M4a => write!(f, "m4a"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioQuality {
    Best,
    Q320,
}

impl Default for AudioQuality {
    fn default() -> Self {
        Self::Best
    }
}

#[derive(Clone)]
pub struct Downloader {
    client: Client,
}

impl Default for Downloader {
    fn default() -> Self {
        Self {
            client: Client::builder()
                .user_agent("syncpulse/0.1.0")
                .timeout(Duration::from_secs(600))
                .build()
                .expect("Failed to build reqwest client"),
        }
    }
}

impl Downloader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn yt_dlp_name() -> &'static str {
        if is_windows() {
            "yt-dlp.exe"
        } else {
            "yt-dlp"
        }
    }

    pub fn yt_dlp_path(config: &Config) -> PathBuf {
        config.bin_dir.join(Self::yt_dlp_name())
    }

    pub fn is_yt_dlp_installed(config: &Config) -> bool {
        Self::yt_dlp_path(config).is_file()
    }

    pub fn ffmpeg_name() -> &'static str {
        if is_windows() {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        }
    }

    pub fn ffprobe_name() -> &'static str {
        if is_windows() {
            "ffprobe.exe"
        } else {
            "ffprobe"
        }
    }

    pub fn ffmpeg_path(config: &Config) -> PathBuf {
        config.bin_dir.join(Self::ffmpeg_name())
    }

    pub fn ffprobe_path(config: &Config) -> PathBuf {
        config.bin_dir.join(Self::ffprobe_name())
    }

    pub fn is_ffmpeg_installed(config: &Config) -> bool {
        Self::ffmpeg_path(config).is_file()
    }

    /// FFmpeg counts as available when SyncPulse manages its own copy or a
    /// working binary is present in `PATH`.
    pub fn is_ffmpeg_available(config: &Config) -> bool {
        if Self::is_ffmpeg_installed(config) {
            return true;
        }
        let name = Self::ffmpeg_name();
        let Some(paths) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&paths).any(|directory| {
            let executable = directory.join(name);
            if !executable.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                executable
                    .metadata()
                    .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                true
            }
        })
    }

    pub async fn ensure_yt_dlp(
        &self,
        config: &Config,
        progress: Option<&Sender<DownloadProgress>>,
    ) -> Result<PathBuf> {
        let path = Self::yt_dlp_path(config);
        if path.is_file() {
            verify_yt_dlp(&path).await?;
            send_progress(progress, 5.0, "yt-dlp already installed");
            return Ok(path);
        }

        fs::create_dir_all(&config.bin_dir)?;
        #[derive(Deserialize)]
        struct Release {
            assets: Vec<Asset>,
        }
        #[derive(Deserialize)]
        struct Asset {
            name: String,
            browser_download_url: String,
        }
        let release: Release = self
            .client
            .get("https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let asset_name = Self::yt_dlp_name();
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| anyhow!("GitHub release does not contain {asset_name}"))?;

        let response = self
            .client
            .get(&asset.browser_download_url)
            .send()
            .await?
            .error_for_status()?;
        let bytes = response.bytes().await?;
        if bytes.is_empty() {
            return Err(anyhow!("Downloaded yt-dlp binary is empty"));
        }
        let suffix = if is_windows() { ".exe" } else { ".download" };
        let mut temporary = TempFileBuilder::new()
            .prefix("yt-dlp-")
            .suffix(suffix)
            .tempfile_in(&config.bin_dir)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        // Persist first, then verify: executing the temporary file while it is
        // still open for writing fails with ETXTBSY ("Text file busy") on Linux.
        temporary
            .persist(&path)
            .with_context(|| format!("Unable to install yt-dlp at {}", path.display()))?;
        if let Err(error) = verify_yt_dlp(&path).await {
            let _ = fs::remove_file(&path);
            return Err(error).context("Downloaded yt-dlp did not pass its version check");
        }
        send_progress(progress, 10.0, "yt-dlp ready");
        Ok(path)
    }

    /// Makes sure a working FFmpeg (and ffprobe) is available locally, fetching
    /// a static build on first use so no system installation is required.
    /// Returns the directory that should be passed to `yt-dlp --ffmpeg-location`.
    pub async fn ensure_ffmpeg(
        &self,
        config: &Config,
        progress: Option<&Sender<DownloadProgress>>,
    ) -> Result<PathBuf> {
        let ffmpeg = Self::ffmpeg_path(config);
        let ffprobe = Self::ffprobe_path(config);
        if ffmpeg.is_file() && ffprobe.is_file() {
            verify_ffmpeg(&ffmpeg).await?;
            send_progress(progress, 5.0, "FFmpeg already installed");
            return Ok(config.bin_dir.clone());
        }

        fs::create_dir_all(&config.bin_dir)?;
        #[derive(Deserialize)]
        struct Index {
            bin: HashMap<String, Platform>,
        }
        #[derive(Deserialize)]
        struct Platform {
            ffmpeg: String,
            ffprobe: String,
        }

        let platform = ffbinaries_platform()?;
        send_progress(progress, 3.0, "Downloading FFmpeg");
        let index: Index = self
            .client
            .get("https://ffbinaries.com/api/v1/version/latest")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let urls = index
            .bin
            .get(platform)
            .ok_or_else(|| anyhow!("FFmpeg binaries are not published for {platform}"))?;

        if !ffmpeg.is_file() {
            install_ffmpeg_binary(&self.client, &urls.ffmpeg, &ffmpeg).await?;
        }
        if !ffprobe.is_file() {
            install_ffmpeg_binary(&self.client, &urls.ffprobe, &ffprobe).await?;
        }

        if let Err(error) = verify_ffmpeg(&ffmpeg).await {
            let _ = fs::remove_file(&ffmpeg);
            let _ = fs::remove_file(&ffprobe);
            return Err(error).context("Downloaded FFmpeg did not pass its version check");
        }
        send_progress(progress, 8.0, "FFmpeg ready");
        Ok(config.bin_dir.clone())
    }

    pub async fn download_to_navidrome(
        &self,
        config: &Config,
        request: &DownloadRequest,
        progress: &Sender<DownloadProgress>,
    ) -> Result<Vec<PathBuf>> {
        if request.url.trim().is_empty() {
            return Err(anyhow!("A media URL is required"));
        }
        if request.format != AudioFormat::Mp3 && request.quality == AudioQuality::Q320 {
            return Err(anyhow!("320 kbps quality is only available for MP3 output"));
        }
        let ffmpeg_dir = self.ensure_ffmpeg(config, Some(progress)).await?;
        let executable = self.ensure_yt_dlp(config, Some(progress)).await?;
        fs::create_dir_all(&config.music_dir)?;
        let music_dir = config.music_dir.canonicalize().with_context(|| {
            format!(
                "Unable to resolve music folder {}",
                config.music_dir.display()
            )
        })?;
        tracing::info!(
            "Starting audio download: format={}, quality={:?}, destination={}",
            request.format,
            request.quality,
            music_dir.display()
        );
        let temp_dir = TempDir::new()?;
        let output_template = temp_dir
            .path()
            .join("%(title)s [%(id)s].%(ext)s")
            .to_string_lossy()
            .into_owned();

        let mut command = Command::new(&executable);
        command
            .args(AUDIO_EXTRACTION_ARGS)
            .arg("--ffmpeg-location")
            .arg(&ffmpeg_dir)
            .arg("-o")
            .arg(output_template)
            .arg("--audio-format")
            .arg(request.format.to_string());
        match (request.format, request.quality) {
            (AudioFormat::Mp3, AudioQuality::Q320) => {
                command.args(["--audio-quality", "320K"]);
            }
            (_, AudioQuality::Best) => {
                command.args(["--audio-quality", "0"]);
            }
            (AudioFormat::Flac | AudioFormat::M4a, AudioQuality::Q320) => {
                return Err(anyhow!("320 kbps quality is only available for MP3 output"));
            }
        }
        command
            .arg("--")
            .arg(&request.url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().context("Unable to start yt-dlp")?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("yt-dlp stdout was not captured"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("yt-dlp stderr was not captured"))?;
        let mut stdout_lines = BufReader::new(stdout).lines();
        let mut stderr_lines = BufReader::new(stderr).lines();
        let mut last_percent = 10.0_f32;
        let mut stdout_done = false;
        let mut stderr_done = false;
        let completion = timeout(Duration::from_secs(900), async {
            while !stdout_done || !stderr_done {
                tokio::select! {
                    line = stdout_lines.next_line(), if !stdout_done => {
                        match line? {
                            Some(line) => {
                                log_yt_dlp_diagnostic(&line);
                                report_line(progress, &line, &mut last_percent)
                            },
                            None => stdout_done = true,
                        }
                    }
                    line = stderr_lines.next_line(), if !stderr_done => {
                        match line? {
                            Some(line) => {
                                log_yt_dlp_diagnostic(&line);
                                report_line(progress, &line, &mut last_percent)
                            },
                            None => stderr_done = true,
                        }
                    }
                }
            }
            child.wait().await
        })
        .await;
        let status = match completion {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                let _ = child.kill().await;
                return Err(error).context("Unable to wait for yt-dlp");
            }
            Err(_) => {
                let _ = child.kill().await;
                return Err(anyhow!("yt-dlp timed out after 15 minutes"));
            }
        };
        let status_success = status.success();

        let mut downloaded = Vec::new();
        for entry in walkdir::WalkDir::new(temp_dir.path()).min_depth(1) {
            let entry = entry?;
            if !entry.file_type().is_file()
                || !has_requested_extension(entry.path(), request.format)
            {
                continue;
            }
            downloaded.push(entry.into_path());
        }
        downloaded.sort();
        tracing::info!(
            "yt-dlp finished with status {}; found {} usable {} file(s) in temporary directory",
            status,
            downloaded.len(),
            request.format
        );
        // A playlist can still exit non-zero because some items were
        // unavailable while the rest downloaded fine, so only fail when
        // nothing usable was produced at all.
        if downloaded.is_empty() {
            if status_success {
                return Err(anyhow!(
                    "yt-dlp completed but produced no {} audio files",
                    request.format
                ));
            }
            return Err(anyhow!("yt-dlp exited with status {status}"));
        }

        let mut saved = Vec::with_capacity(downloaded.len());
        let mut failed = 0_usize;
        for source in downloaded {
            match copy_without_overwrite(&source, &music_dir) {
                Ok(path) => {
                    let size = fs::metadata(&path)
                        .map(|metadata| metadata.len())
                        .unwrap_or(0);
                    tracing::info!("Saved audio file {} ({} bytes)", path.display(), size);
                    saved.push(path)
                }
                // A single unwritable file must not discard the rest of the
                // batch, which would otherwise disappear with the temp folder.
                Err(error) => {
                    failed += 1;
                    tracing::warn!(
                        "Could not save downloaded file {}: {error:#}",
                        source.display()
                    );
                }
            }
        }
        if saved.is_empty() {
            return Err(anyhow!(
                "Downloaded {failed} audio file(s) but none could be saved to {}",
                config.music_dir.display()
            ));
        }
        tracing::info!(
            "Audio download saved {} file(s) to {}",
            saved.len(),
            music_dir.display()
        );

        let skipped = match (status_success, failed) {
            (true, 0) => "",
            (false, 0) => " (some items were unavailable)",
            (true, _) => " (some files could not be saved)",
            (false, _) => " (some items were unavailable, some files could not be saved)",
        };
        let message = match navidrome_credentials(config) {
            None => {
                tracing::info!(
                    "Skipping immediate Navidrome rescan because no scan account is configured; files saved under {}",
                    music_dir.display()
                );
                format!(
                    "Download saved{skipped}; add a Navidrome account in Settings to rescan immediately"
                )
            }
            Some(_) => match self.request_library_scan(config).await {
                Ok(scan) => format!("Download saved{skipped}; {scan}"),
                Err(error) => {
                    tracing::warn!("Downloaded files, but the Navidrome rescan failed: {error:#}");
                    format!("Download saved{skipped}; Navidrome rescan failed: {error:#}")
                }
            },
        };
        send_progress(Some(progress), 100.0, &message);
        Ok(saved)
    }

    /// Asks Navidrome to rescan its library immediately through the Subsonic
    /// API (`startScan`). Requires a Navidrome account, which the user can add
    /// in Settings; without one Navidrome still scans on its own schedule.
    pub async fn request_library_scan(&self, config: &Config) -> Result<&'static str> {
        let Some((username, password)) = navidrome_credentials(config) else {
            return Err(anyhow!(
                "Add a Navidrome username and password in Settings to rescan the library on demand"
            ));
        };
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).context("Unable to generate a Subsonic auth salt")?;
        let salt: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let token = subsonic_token(password, &salt);
        let url = format!("http://127.0.0.1:{}/rest/startScan", config.navidrome_port);
        tracing::info!(
            "Requesting Navidrome library scan at 127.0.0.1:{} for configured music folder {}",
            config.navidrome_port,
            config.music_dir.display()
        );
        let response = timeout(
            Duration::from_secs(15),
            self.client
                .get(&url)
                .query(&[
                    ("u", username),
                    ("t", token.as_str()),
                    ("s", salt.as_str()),
                    ("v", "1.16.1"),
                    ("c", "SyncPulse"),
                    ("f", "json"),
                ])
                .send(),
        )
        .await
        .context("Navidrome rescan request timed out")?
        .context("Unable to reach Navidrome to request a library rescan")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            tracing::warn!("Navidrome library scan request returned HTTP {status}");
            return Err(anyhow!("Navidrome returned HTTP {status}"));
        }
        let envelope: SubsonicEnvelope = serde_json::from_str(body.trim()).with_context(|| {
            format!("Navidrome returned an unexpected response: {}", body.trim())
        })?;
        if let Some(error) = envelope.response.error {
            tracing::warn!(
                "Navidrome rejected library scan request: {} (code {})",
                error.message,
                error.code
            );
            return Err(anyhow!("{} (code {})", error.message, error.code));
        }
        tracing::info!("Navidrome accepted the library scan request");
        Ok("Navidrome library rescan requested")
    }
}

/// Returns the configured Navidrome account, treating blank values as unset.
fn navidrome_credentials(config: &Config) -> Option<(&str, &str)> {
    let username = config.navidrome_username.as_deref()?.trim();
    let password = config.navidrome_password.as_deref()?.trim();
    (!username.is_empty() && !password.is_empty()).then_some((username, password))
}

/// Subsonic token authentication: lowercase hex `md5(password + salt)`.
fn subsonic_token(password: &str, salt: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(password.as_bytes());
    hasher.update(salt.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Deserialize)]
struct SubsonicEnvelope {
    #[serde(rename = "subsonic-response")]
    response: SubsonicResponse,
}

#[derive(Deserialize)]
struct SubsonicResponse {
    #[serde(default)]
    error: Option<SubsonicError>,
}

#[derive(Deserialize)]
struct SubsonicError {
    code: u32,
    message: String,
}

async fn verify_yt_dlp(executable: &Path) -> Result<()> {
    let output = timeout(
        Duration::from_secs(15),
        Command::new(executable)
            .arg("--version")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .context("yt-dlp version check timed out")?
    .with_context(|| format!("Unable to run yt-dlp at {}", executable.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "yt-dlp version check failed at {}: {}",
            executable.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if String::from_utf8_lossy(&output.stdout).trim().is_empty() {
        return Err(anyhow!(
            "yt-dlp version check returned no version information at {}",
            executable.display()
        ));
    }
    Ok(())
}

async fn verify_ffmpeg(executable: &Path) -> Result<()> {
    let output = timeout(
        Duration::from_secs(10),
        Command::new(executable)
            .arg("-version")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .context("FFmpeg version check timed out")?
    .with_context(|| format!("Unable to run FFmpeg at {}", executable.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "FFmpeg version check failed at {}: {}",
            executable.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn ffbinaries_platform() -> Result<&'static str> {
    ffbinaries_platform_for(detect_os_arch())
}

fn ffbinaries_platform_for(arch: OsArch) -> Result<&'static str> {
    match arch {
        OsArch::LinuxX86_64 => Ok("linux-64"),
        OsArch::LinuxAArch64 => Ok("linux-arm64"),
        OsArch::WindowsX86_64 => Ok("windows-64"),
        OsArch::WindowsAArch64 => Err(anyhow!(
            "FFmpeg builds are not published for Windows on ARM; install FFmpeg manually and add it to PATH"
        )),
        OsArch::Unknown => Err(anyhow!(
            "FFmpeg auto-install is not supported on this platform; install FFmpeg manually and add it to PATH"
        )),
    }
}

async fn install_ffmpeg_binary(client: &Client, url: &str, destination: &Path) -> Result<()> {
    let bytes = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let member = destination
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("Invalid FFmpeg destination {}", destination.display()))?;
    extract_zip_binary(&bytes, member, destination)
}

fn extract_zip_binary(archive_bytes: &[u8], member: &str, destination: &Path) -> Result<()> {
    let mut archive = ZipArchive::new(Cursor::new(archive_bytes))
        .context("Downloaded FFmpeg archive is invalid")?;
    let mut found = None;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        if entry.is_file() && entry.name().ends_with(member) {
            found = Some(index);
            break;
        }
    }
    let index = found.ok_or_else(|| anyhow!("FFmpeg archive does not contain {member}"))?;
    let directory = destination.parent().ok_or_else(|| {
        anyhow!(
            "FFmpeg destination has no directory: {}",
            destination.display()
        )
    })?;

    let mut entry = archive.by_index(index)?;
    let mut temporary = TempFileBuilder::new()
        .prefix("ffmpeg-")
        .suffix(if is_windows() { ".exe" } else { ".download" })
        .tempfile_in(directory)?;
    copy(&mut entry, temporary.as_file_mut())?;
    temporary.as_file_mut().flush()?;
    temporary.as_file().sync_all()?;
    if temporary.as_file().metadata()?.len() == 0 {
        return Err(anyhow!("FFmpeg archive member {member} is empty"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    temporary
        .persist(destination)
        .with_context(|| format!("Unable to install FFmpeg at {}", destination.display()))?;
    Ok(())
}

fn has_requested_extension(path: &Path, format: AudioFormat) -> bool {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    match format {
        AudioFormat::Mp3 => extension.eq_ignore_ascii_case("mp3"),
        AudioFormat::Flac => extension.eq_ignore_ascii_case("flac"),
        AudioFormat::M4a => extension.eq_ignore_ascii_case("m4a"),
    }
}

fn copy_without_overwrite(source: &Path, destination_dir: &Path) -> Result<PathBuf> {
    let file_name = source
        .file_name()
        .ok_or_else(|| anyhow!("Downloaded file has no name: {}", source.display()))?;
    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("track");
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let mut temporary = tempfile::NamedTempFile::new_in(destination_dir)?;
    copy(&mut File::open(source)?, temporary.as_file_mut())
        .with_context(|| format!("Unable to copy downloaded file {}", source.display()))?;
    temporary.as_file_mut().flush()?;
    temporary.as_file().sync_all()?;

    let mut index = 0_u32;
    loop {
        let destination = if index == 0 {
            destination_dir.join(file_name)
        } else {
            destination_dir.join(format!("{stem}-{index}.{extension}"))
        };
        match temporary.persist_noclobber(&destination) {
            Ok(_) => return Ok(destination),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                temporary = error.file;
                index = index
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("Too many files with the same name"))?;
            }
            Err(error) => {
                return Err(error.error).with_context(|| {
                    format!(
                        "Unable to save downloaded file to {}",
                        destination.display()
                    )
                });
            }
        }
    }
}

fn extract_percent(line: &str) -> Option<f32> {
    static PERCENT_REGEX: OnceLock<Regex> = OnceLock::new();
    let regex = PERCENT_REGEX
        .get_or_init(|| Regex::new(r"(\d{1,3}(?:\.\d+)?)%").expect("valid progress regex"));
    let percent = regex.captures(line)?.get(1)?.as_str().parse::<f32>().ok()?;
    (0.0..=100.0).contains(&percent).then_some(percent)
}

fn report_line(tx: &Sender<DownloadProgress>, line: &str, last_percent: &mut f32) {
    if let Some(percent) = extract_percent(line) {
        *last_percent = (10.0 + percent * 0.7).clamp(*last_percent, 80.0);
    }
    send_progress(Some(tx), *last_percent, line);
}

fn log_yt_dlp_diagnostic(line: &str) {
    let trimmed = line.trim();
    if trimmed.contains("[ERROR]") || trimmed.starts_with("ERROR:") {
        tracing::warn!("yt-dlp: {trimmed}");
    } else if trimmed.contains("[warning]") || trimmed.starts_with("WARNING:") {
        tracing::info!("yt-dlp: {trimmed}");
    }
}

fn send_progress(tx: Option<&Sender<DownloadProgress>>, percent: f32, message: &str) {
    if let Some(tx) = tx {
        let _ = tx.send(DownloadProgress {
            percent,
            message: message.to_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        copy_without_overwrite, extract_percent, extract_zip_binary, ffbinaries_platform_for,
        has_requested_extension, subsonic_token, verify_yt_dlp, AudioFormat, Regex,
        AUDIO_EXTRACTION_ARGS, TITLE_CLEANUP_PATTERN,
    };
    use std::fs;
    use std::path::Path;

    #[test]
    fn subsonic_token_is_the_md5_of_password_followed_by_salt() {
        // Standard MD5 test vector: md5("abc").
        assert_eq!(
            subsonic_token("abc", ""),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        // The token must depend on the salt, otherwise Navidrome rejects it.
        assert_ne!(
            subsonic_token("abc", "salt"),
            subsonic_token("abc", "salt2")
        );
    }

    #[test]
    fn progress_parser_rejects_invalid_percentages() {
        assert_eq!(extract_percent("[download] 42.5% of 10MiB"), Some(42.5));
        assert_eq!(extract_percent("100%"), Some(100.0));
        assert_eq!(extract_percent("101%"), None);
        assert_eq!(extract_percent("no progress"), None);
    }

    #[test]
    fn output_filter_matches_only_requested_audio_format() {
        assert!(has_requested_extension(
            Path::new("track.MP3"),
            AudioFormat::Mp3
        ));
        assert!(has_requested_extension(
            Path::new("track.m4a"),
            AudioFormat::M4a
        ));
        assert!(!has_requested_extension(
            Path::new("track.mp4"),
            AudioFormat::M4a
        ));
    }

    #[test]
    fn audio_download_explicitly_processes_playlists_and_embeds_tags_and_artwork() {
        assert!(AUDIO_EXTRACTION_ARGS.contains(&"--yes-playlist"));
        assert!(AUDIO_EXTRACTION_ARGS.contains(&"--embed-metadata"));
        assert!(AUDIO_EXTRACTION_ARGS.contains(&"--embed-thumbnail"));
        assert!(AUDIO_EXTRACTION_ARGS.contains(&"--extract-audio"));
        assert!(AUDIO_EXTRACTION_ARGS.contains(&"--ignore-errors"));
        assert!(AUDIO_EXTRACTION_ARGS.contains(&"--replace-in-metadata"));
        assert!(AUDIO_EXTRACTION_ARGS.contains(&"--js-runtimes"));
        assert!(AUDIO_EXTRACTION_ARGS.contains(&"--parse-metadata"));
        assert!(AUDIO_EXTRACTION_ARGS.contains(&"%(uploader,channel,creator)s:artist"));
        assert!(AUDIO_EXTRACTION_ARGS.contains(&"%(uploader,channel,creator)s:album_artist"));
        assert!(AUDIO_EXTRACTION_ARGS.contains(&"%(playlist_title,title)s:album"));
        assert!(!AUDIO_EXTRACTION_ARGS.contains(&"%(playlist_title|YouTube Downloads)s:album"));
    }

    #[test]
    fn title_cleanup_keeps_the_song_name_and_drops_uploader_clutter() {
        let pattern = Regex::new(TITLE_CLEANUP_PATTERN).expect("valid title cleanup regex");
        let clean = |title: &str| pattern.replace_all(title, "").trim().to_string();

        assert_eq!(
            clean("4rd Sanctuary [John Cover] | Deltarune Chapter 5"),
            "4rd Sanctuary"
        );
        assert_eq!(
            clean("GIGA プリン [John Cover v2 Ft. Kasane Teto] ｜ Deltarune: The Other Puppet Chapter 2"),
            "GIGA プリン"
        );
        assert_eq!(
            clean("Rick Astley - Never Gonna Give You Up (Official Video)"),
            "Rick Astley - Never Gonna Give You Up"
        );
        assert_eq!(clean("Bohemian Rhapsody"), "Bohemian Rhapsody");
    }

    #[test]
    fn saving_duplicate_track_names_never_overwrites_existing_music() {
        let first_source_dir = tempfile::tempdir().expect("first source directory");
        let second_source_dir = tempfile::tempdir().expect("second source directory");
        let music_dir = tempfile::tempdir().expect("music directory");
        let first_source = first_source_dir.path().join("track.mp3");
        let second_source = second_source_dir.path().join("track.mp3");
        fs::write(&first_source, b"first").expect("first source");
        fs::write(&second_source, b"second").expect("second source");

        let first = copy_without_overwrite(&first_source, music_dir.path())
            .expect("first track should be saved");
        let second = copy_without_overwrite(&second_source, music_dir.path())
            .expect("duplicate should be saved under a different name");

        assert_eq!(fs::read(first).expect("first track data"), b"first");
        assert_eq!(fs::read(second).expect("second track data"), b"second");
        assert!(music_dir.path().join("track-1.mp3").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn yt_dlp_installation_check_requires_a_working_version_command() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary tool directory");
        let valid = directory.path().join("yt-dlp-valid");
        fs::write(&valid, "#!/bin/sh\nprintf '2026.01.01\\n'\n").unwrap();
        fs::set_permissions(&valid, fs::Permissions::from_mode(0o755)).unwrap();
        verify_yt_dlp(&valid).await.unwrap();

        let invalid = directory.path().join("yt-dlp-invalid");
        fs::write(&invalid, "#!/bin/sh\nexit 2\n").unwrap();
        fs::set_permissions(&invalid, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_yt_dlp(&invalid).await.is_err());
    }

    #[test]
    fn ffmpeg_platform_mapping_covers_the_supported_targets() {
        use crate::utils::OsArch;

        assert_eq!(
            ffbinaries_platform_for(OsArch::LinuxX86_64).unwrap(),
            "linux-64"
        );
        assert_eq!(
            ffbinaries_platform_for(OsArch::LinuxAArch64).unwrap(),
            "linux-arm64"
        );
        assert_eq!(
            ffbinaries_platform_for(OsArch::WindowsX86_64).unwrap(),
            "windows-64"
        );
        assert!(ffbinaries_platform_for(OsArch::WindowsAArch64).is_err());
        assert!(ffbinaries_platform_for(OsArch::Unknown).is_err());
    }

    #[test]
    fn ffmpeg_archive_member_is_installed_at_its_destination() {
        use std::io::{Cursor, Write};
        use zip::write::SimpleFileOptions;

        let mut buffer = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut buffer));
            writer
                .start_file("ffmpeg", SimpleFileOptions::default())
                .expect("start zip member");
            writer
                .write_all(b"ffmpeg-binary")
                .expect("write zip member");
            writer.finish().expect("finish zip archive");
        }

        let directory = tempfile::tempdir().expect("temporary tool directory");
        let destination = directory.path().join("ffmpeg");
        extract_zip_binary(&buffer, "ffmpeg", &destination).expect("extract the ffmpeg member");
        assert_eq!(
            fs::read(&destination).expect("installed binary"),
            b"ffmpeg-binary"
        );
        assert!(extract_zip_binary(&buffer, "ffmpeg.exe", &destination).is_err());
    }
}
