use std::{
    io,
    os::unix::process::CommandExt,
    process::{Child, Command},
};

#[cfg(target_os = "linux")]
use std::fs;

use super::ProcessGroup;

pub(super) struct OsProcessGroup {
    pgid: Option<libc::pid_t>,
}

impl OsProcessGroup {
    pub(super) fn new() -> Self {
        Self { pgid: None }
    }
}

impl ProcessGroup for OsProcessGroup {
    fn spawn_in_group(&mut self, command: &mut Command) -> io::Result<Child> {
        // SAFETY: setsid is async-signal-safe and touches no Rust-managed memory.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        self.pgid = Some(child.id() as libc::pid_t);
        Ok(child)
    }

    fn terminate(&mut self) -> io::Result<()> {
        let Some(pgid) = self.pgid else {
            return Ok(());
        };
        // SAFETY: killpg does not dereference pointers; pgid was returned by spawn.
        if unsafe { libc::killpg(pgid, libc::SIGTERM) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }

    fn members(&self) -> io::Result<Vec<u32>> {
        let Some(pgid) = self.pgid else {
            return Ok(Vec::new());
        };
        members_of_group(pgid)
    }
}

#[cfg(target_os = "linux")]
fn members_of_group(pgid: libc::pid_t) -> io::Result<Vec<u32>> {
    let mut members = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(after_name) = stat.rsplit_once(") ").map(|(_, fields)| fields) else {
            continue;
        };
        let fields: Vec<_> = after_name.split_whitespace().collect();
        // Fields after comm start at process state: state, ppid, pgrp, ...
        if fields.get(2).and_then(|value| value.parse().ok()) == Some(pgid)
            && fields.first() != Some(&"Z")
        {
            members.push(pid);
        }
    }
    members.sort_unstable();
    Ok(members)
}

#[cfg(not(target_os = "linux"))]
fn members_of_group(pgid: libc::pid_t) -> io::Result<Vec<u32>> {
    // SAFETY: signal 0 only probes existence and permissions.
    if unsafe { libc::killpg(pgid, 0) } == 0 {
        Ok(vec![pgid as u32])
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(Vec::new())
        } else {
            Err(error)
        }
    }
}
