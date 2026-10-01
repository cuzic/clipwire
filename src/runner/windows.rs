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
    fn spawn_in_group(&mut self, _command: &mut Command) -> io::Result<Child> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows Job Object support is not implemented yet",
        ))
    }

    fn terminate(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows Job Object support is not implemented yet",
        ))
    }

    fn members(&self) -> io::Result<Vec<u32>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows Job Object support is not implemented yet",
        ))
    }
}
