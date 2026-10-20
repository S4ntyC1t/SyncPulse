#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub fn open(url: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();

    result
        .map(|_| ())
        .map_err(|error| format!("Unable to open the system browser: {error}"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn open(_url: &str) -> Result<(), String> {
    Err("Opening the system browser is not supported on this platform".to_owned())
}
