use std::env;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsArch {
    LinuxX86_64,
    LinuxAArch64,
    WindowsX86_64,
    WindowsAArch64,
    Unknown,
}
impl std::fmt::Display for OsArch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OsArch::LinuxX86_64 => write!(f, "linux-x86_64"),
            OsArch::LinuxAArch64 => write!(f, "linux-aarch64"),
            OsArch::WindowsX86_64 => write!(f, "windows-x86_64"),
            OsArch::WindowsAArch64 => write!(f, "windows-aarch64"),
            OsArch::Unknown => write!(f, "unknown"),
        }
    }
}

pub fn detect_os_arch() -> OsArch {
    let os = env::consts::OS;
    let arch = env::consts::ARCH;
    match (os, arch) {
        ("linux", "x86_64") => OsArch::LinuxX86_64,
        ("linux", "aarch64") => OsArch::LinuxAArch64,
        ("windows", "x86_64") => OsArch::WindowsX86_64,
        ("windows", "aarch64") => OsArch::WindowsAArch64,
        _ => OsArch::Unknown,
    }
}
pub fn is_windows() -> bool {
    cfg!(target_os = "windows")
}
#[cfg(target_os = "windows")]
pub fn spawn_detached_no_window(
    cmd: &str,
    args: &[&str],
    cwd: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let mut c = Command::new(cmd);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    c.args(args).creation_flags(0x08000000).spawn()?;
    Ok(())
}
#[cfg(not(target_os = "windows"))]
pub fn spawn_detached_no_window(
    cmd: &str,
    args: &[&str],
    cwd: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let mut c = Command::new(cmd);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    c.args(args).spawn()?;
    Ok(())
}

/// Periodically returns freed heap pages to the operating system.
///
/// glibc keeps freed allocations in its arenas; a long-running GUI shell that
/// repeatedly allocates buffers (JSON payloads, HTTP responses, process scans)
/// can end up holding tens of megabytes that it never uses again. `malloc_trim`
/// hands those pages back so RSS follows real usage instead of the historical
/// high-water mark. Safe to call more than once; a no-op off glibc/Linux.
pub fn start_memory_maintenance() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if STARTED.set(()).is_err() {
            return;
        }
        let _ = std::thread::Builder::new()
            .name("memory-maintenance".to_owned())
            .spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(30));
                // SAFETY: malloc_trim only inspects and releases the calling
                // process' own allocator arenas and is thread-safe.
                unsafe {
                    libc::malloc_trim(0);
                }
            });
    }
}
