//! Native process identities shared by execution records and controller ownership.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started: u64,
    #[serde(default)]
    pub birth: String,
    pub executable: PathBuf,
}
impl ProcessIdentity {
    pub fn read(pid: u32) -> Option<Self> {
        let mut system = System::new();
        let ids = [Pid::from_u32(pid)];
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&ids),
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
        );
        let process = system.process(ids[0])?;
        if matches!(
            process.status(),
            sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
        ) {
            return None;
        }
        Some(Self {
            pid,
            started: process.start_time(),
            birth: birth(pid, process.start_time())?,
            executable: process.exe()?.to_path_buf(),
        })
    }
    pub fn is_alive(&self) -> bool {
        Self::read(self.pid).as_ref() == Some(self)
    }
    pub fn terminate(&self) -> anyhow::Result<bool> {
        if !self.is_alive() {
            return Ok(false);
        }
        #[cfg(unix)]
        {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            )?;
            Ok(true)
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("Graceful termination is unavailable on this platform")
        }
    }
}

fn birth(pid: u32, started: u64) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let _ = started;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let ticks = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        Some(format!("{}:{ticks}", boot.trim()))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = started;
        let mut info = std::mem::MaybeUninit::<nix::libc::proc_bsdinfo>::uninit();
        let size = std::mem::size_of::<nix::libc::proc_bsdinfo>() as i32;
        // The kernel fills exactly this ABI-defined buffer for our process.
        let written = unsafe {
            nix::libc::proc_pidinfo(
                pid as i32,
                nix::libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written != size {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some(format!(
            "{}:{}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        Some(started.to_string())
    }
}
