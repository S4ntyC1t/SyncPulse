use anyhow::{anyhow, Result};
use std::path::Path;
use sysinfo::{Pid, ProcessesToUpdate, System};

#[derive(Clone, Debug)]
pub struct RunningProc {
    pub pid: u32,
}

pub struct ProcessManager {
    sys: System,
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            sys: System::new_all(),
        }
    }

    fn refresh(&mut self) {
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
    }

    pub fn find_managed_processes(
        &mut self,
        navidrome_executable: &Path,
        discord_rpc_executable: &Path,
    ) -> (Option<RunningProc>, Option<RunningProc>) {
        self.refresh();
        (
            find_process_at(&self.sys, navidrome_executable),
            find_discord_rpc_process_at(&self.sys, discord_rpc_executable),
        )
    }

    pub fn stop_navidrome_at(&mut self, executable: &Path) -> Result<bool> {
        self.stop_path(executable)
    }

    fn stop_path(&mut self, executable: &Path) -> Result<bool> {
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
