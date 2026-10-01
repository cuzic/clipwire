use std::{
    io,
    process::{Child, Command},
};

use super::ProcessGroup;

pub(super) struct OsProcessGroup;

impl OsProcessGroup {
    pub(super) fn new() -> Self {
        Self
    }
}

impl ProcessGroup for OsProcessGroup {
    fn spawn_in_group(&mut self, command: &mut Command) -> io::Result<Child> {
        // Job Object integration belongs to T4.4. Keep the pre-Runner
        // std::process behavior as the explicit Windows fallback for T4.5.
        command.spawn()
    }

    fn terminate(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn members(&self) -> io::Result<Vec<u32>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows Job Object support is not implemented yet",
        ))
    }
}
