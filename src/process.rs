use anyhow::{anyhow, Result};
use std::path::Path;
#[cfg(not(target_os = "linux"))]
use sysinfo::{Pid, ProcessesToUpdate, System};

#[derive(Clone, Debug)]
pub struct RunningProc {
    pub pid: u32,
}

pub struct ProcessManager {
    #[cfg(not(target_os = "linux"))]
    sys: System,
}

impl ProcessManager {
    pub fn new() -> Self {
        #[cfg(not(target_os = "linux"))]
        {
            Self { sys: System::new() }
        }
        #[cfg(target_os = "linux")]
        {
            Self {}
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn refresh(&mut self) {
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
    }

    pub fn find_managed_processes(
        &mut self,
        navidrome_executable: &Path,
        discord_rpc_executable: &Path,
    ) -> (Option<RunningProc>, Option<RunningProc>) {
        #[cfg(target_os = "linux")]
        {
            return linux_find_managed_processes(navidrome_executable, discord_rpc_executable);
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.refresh();
            (
                find_process_at(&self.sys, navidrome_executable),
                find_discord_rpc_process_at(&self.sys, discord_rpc_executable),
            )
        }
    }

    pub fn stop_navidrome_at(&mut self, executable: &Path) -> Result<bool> {
        self.stop_path(executable)
    }

    fn stop_path(&mut self, executable: &Path) -> Result<bool> {
        #[cfg(target_os = "linux")]
        {
            let Some(process) = linux_find_process_at(executable) else {
                return Ok(false);
            };
            // SAFETY: kill is called with a PID read from /proc and SIGTERM is
            // the same graceful termination used by sysinfo on other targets.
            let result = unsafe { libc::kill(process.pid as libc::pid_t, libc::SIGTERM) };
            if result != 0 {
                return Err(anyhow!(
                    "Failed to send termination signal to PID {}",
                    process.pid
                ));
            }
            return Ok(true);
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.refresh();
            let Some(process) = find_process_at(&self.sys, executable) else {
                return Ok(false);
            };
            let pid = Pid::from_u32(process.pid);
            let Some(process) = self.sys.process(pid) else {
                return Ok(false);
            };
            if !process.kill() {
                return Err(anyhow!("Failed to send termination signal to PID {}", pid));
            }
            Ok(true)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn find_discord_rpc_process_at(sys: &System, executable: &Path) -> Option<RunningProc> {
    for (pid, process) in sys.processes() {
        if process
            .exe()
            .is_some_and(|path| same_path(path, executable))
            && !process
                .cmd()
                .iter()
                .any(|argument| argument.to_string_lossy() == "serve-covers")
            && !process
                .environ()
                .iter()
                .any(|entry| entry.to_string_lossy().starts_with("COVERS_LISTEN="))
        {
            return Some(RunningProc { pid: pid.as_u32() });
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn find_process_at(sys: &System, executable: &Path) -> Option<RunningProc> {
    for (pid, process) in sys.processes() {
        let Some(process_executable) = process.exe().or_else(|| {
            process
                .cmd()
                .first()
                .and_then(|argument| argument.to_str())
                .map(Path::new)
        }) else {
            continue;
        };
        if same_path(process_executable, executable) {
            return Some(RunningProc { pid: pid.as_u32() });
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn linux_find_process_at(executable: &Path) -> Option<RunningProc> {
    let expected = canonical_path(executable);
    std::fs::read_dir("/proc")
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let root = entry.path();
            let actual = std::fs::read_link(root.join("exe")).ok()?;
            (actual == expected && !linux_process_is_zombie(&root)).then_some(RunningProc { pid })
        })
}

#[cfg(target_os = "linux")]
fn linux_find_managed_processes(
    navidrome_executable: &Path,
    discord_rpc_executable: &Path,
) -> (Option<RunningProc>, Option<RunningProc>) {
    let navidrome_expected = canonical_path(navidrome_executable);
    let rpc_expected = canonical_path(discord_rpc_executable);
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return (None, None);
    };
    let mut navidrome = None;
    let mut discord_rpc = None;
    for entry in entries.filter_map(Result::ok) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let root = entry.path();
        let Ok(actual) = std::fs::read_link(root.join("exe")) else {
            continue;
        };
        if linux_process_is_zombie(&root) {
            continue;
        }
        if navidrome.is_none() && actual == navidrome_expected {
            navidrome = Some(RunningProc { pid });
        }
        if discord_rpc.is_none() && actual == rpc_expected {
            // Only inspect command line/environment for the configured RPC
            // executable. Reading every process environment on startup is
            // needlessly expensive and can be large on developer workstations.
            let args = read_proc_strings(root.join("cmdline"));
            let env = read_proc_strings(root.join("environ"));
            if !args.iter().any(|argument| argument == "serve-covers")
                && !env.iter().any(|entry| entry.starts_with("COVERS_LISTEN="))
            {
                discord_rpc = Some(RunningProc { pid });
            }
        }
        if navidrome.is_some() && discord_rpc.is_some() {
            break;
        }
    }
    (navidrome, discord_rpc)
}

#[cfg(target_os = "linux")]
fn read_proc_strings(path: std::path::PathBuf) -> Vec<String> {
    std::fs::read(path)
        .unwrap_or_default()
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

#[cfg(target_os = "linux")]
fn linux_process_is_zombie(root: &std::path::Path) -> bool {
    std::fs::read_to_string(root.join("stat"))
        .ok()
        .and_then(|stat| stat.rfind(')').map(|close| stat[close + 1..].to_owned()))
        .and_then(|rest| rest.split_whitespace().next().map(str::to_owned))
        .is_some_and(|state| state == "Z")
}

#[cfg(target_os = "linux")]
fn canonical_path(path: &Path) -> std::path::PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(not(target_os = "linux"))]
fn same_path(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn linux_process_scan_finds_the_current_process() {
        let executable = std::env::current_exe().unwrap();
        let process = super::linux_find_process_at(&executable).unwrap();
        assert_eq!(process.pid, std::process::id());
    }
}
