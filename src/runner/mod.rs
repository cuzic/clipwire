//! 子プロセス実行の共通 API と OS ごとのプロセスグループ抽象。

use std::{
    ffi::OsString,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

#[cfg(not(windows))]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
use unix::OsProcessGroup;
#[cfg(windows)]
use windows::OsProcessGroup;

static NEXT_LOG_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub(crate) struct JobSpec {
    pub(crate) command: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) requester: String,
}

impl JobSpec {
    pub(crate) fn new(command: impl Into<PathBuf>, requester: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            timeout: None,
            requester: requester.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobState {
    Running,
    Succeeded,
    Failed,
    SpawnFailed,
}

pub(crate) trait ProcessGroup: Send {
    fn spawn_in_group(&mut self, command: &mut Command) -> io::Result<Child>;
    fn terminate(&mut self) -> io::Result<()>;
    fn members(&self) -> io::Result<Vec<u32>>;
}

pub(crate) struct Runner {
    log_dir: PathBuf,
}

impl Runner {
    pub(crate) fn new(log_dir: impl Into<PathBuf>) -> Self {
        Self {
            log_dir: log_dir.into(),
        }
    }

    pub(crate) fn spawn(&self, spec: JobSpec) -> io::Result<JobHandle> {
        fs::create_dir_all(&self.log_dir)?;
        let log_path = self.next_log_path();
        let log = File::create(&log_path)?;
        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }

        let mut group = OsProcessGroup::new();
        match group.spawn_in_group(&mut command) {
            Ok(child) => Ok(JobHandle {
                child: Some(child),
                group: Box::new(group),
                log_path,
                state: JobState::Running,
                exit_code: None,
                spawn_error: None,
                spec,
            }),
            Err(error) => Ok(JobHandle {
                child: None,
                group: Box::new(group),
                log_path,
                state: JobState::SpawnFailed,
                exit_code: None,
                spawn_error: Some(error),
                spec,
            }),
        }
    }

    fn next_log_path(&self) -> PathBuf {
        let id = NEXT_LOG_ID.fetch_add(1, Ordering::Relaxed);
        self.log_dir
            .join(format!("{}-{id}.log", std::process::id()))
    }
}

pub(crate) struct JobHandle {
    child: Option<Child>,
    group: Box<dyn ProcessGroup>,
    log_path: PathBuf,
    state: JobState,
    exit_code: Option<i32>,
    spawn_error: Option<io::Error>,
    spec: JobSpec,
}

impl JobHandle {
    pub(crate) fn wait(&mut self) -> io::Result<JobState> {
        let Some(child) = &mut self.child else {
            return Ok(self.state);
        };
        let status = child.wait()?;
        self.record_status(status);
        Ok(self.state)
    }

    pub(crate) fn terminate(&mut self) -> io::Result<()> {
        self.group.terminate()
    }

    pub(crate) fn members(&self) -> io::Result<Vec<u32>> {
        self.group.members()
    }

    pub(crate) fn log_path(&self) -> &Path {
        &self.log_path
    }

    pub(crate) fn state(&self) -> JobState {
        self.state
    }

    pub(crate) fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    pub(crate) fn spawn_error(&self) -> Option<&io::Error> {
        self.spawn_error.as_ref()
    }

    pub(crate) fn spec(&self) -> &JobSpec {
        &self.spec
    }

    fn record_status(&mut self, status: ExitStatus) {
        self.exit_code = status.code();
        self.state = if status.success() {
            JobState::Succeeded
        } else {
            JobState::Failed
        };
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{thread, time::Instant};

    fn runner() -> (tempfile::TempDir, Runner) {
        let temp = tempfile::tempdir().unwrap();
        let runner = Runner::new(temp.path());
        (temp, runner)
    }

    #[test]
    fn reports_success_failure_and_spawn_failure() {
        let (_temp, runner) = runner();

        let mut echo = JobSpec::new("echo", "test");
        echo.args.push("hello".into());
        let mut echo = runner.spawn(echo).unwrap();
        assert_eq!(echo.wait().unwrap(), JobState::Succeeded);
        assert_eq!(echo.exit_code(), Some(0));
        assert_eq!(fs::read_to_string(echo.log_path()).unwrap(), "hello\n");

        let mut false_job = runner.spawn(JobSpec::new("false", "test")).unwrap();
        assert_eq!(false_job.wait().unwrap(), JobState::Failed);
        assert_ne!(false_job.exit_code(), Some(0));

        let missing = runner
            .spawn(JobSpec::new("clipwire-command-that-does-not-exist", "test"))
            .unwrap();
        assert_eq!(missing.state(), JobState::SpawnFailed);
        assert_eq!(missing.exit_code(), None);
        assert_eq!(
            missing.spawn_error().unwrap().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn terminate_kills_the_entire_process_group() {
        let (_temp, runner) = runner();
        let mut spec = JobSpec::new("sh", "test");
        spec.args = ["-c", "sleep 100 & sleep 100"]
            .into_iter()
            .map(Into::into)
            .collect();
        let mut job = runner.spawn(spec).unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let members = loop {
            let members = job.members().unwrap();
            if members.len() >= 3 {
                break members;
            }
            assert!(
                Instant::now() < deadline,
                "孫を含むプロセスが起動しなかった"
            );
            thread::sleep(Duration::from_millis(20));
        };

        job.terminate().unwrap();
        assert_eq!(job.wait().unwrap(), JobState::Failed);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let remaining = job.members().unwrap();
            if remaining.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "終了後もプロセスが残っている: {remaining:?} (開始時: {members:?})"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}
