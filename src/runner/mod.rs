//! 子プロセス実行の共通 API と OS ごとのプロセスグループ抽象。

use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
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
    pub(crate) env: Vec<(OsString, OsString)>,
}

impl JobSpec {
    pub(crate) fn new(command: impl Into<PathBuf>, requester: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            timeout: None,
            requester: requester.into(),
            env: Vec::new(),
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
    output: Option<OrderedOutput>,
}

impl Runner {
    pub(crate) fn new(log_dir: impl Into<PathBuf>) -> Self {
        Self {
            log_dir: log_dir.into(),
            output: None,
        }
    }

    pub(crate) fn with_output(log_dir: impl Into<PathBuf>, output: OrderedOutput) -> Self {
        Self {
            log_dir: log_dir.into(),
            output: Some(output),
        }
    }

    pub(crate) fn spawn(&self, spec: JobSpec) -> io::Result<JobHandle> {
        fs::create_dir_all(&self.log_dir)?;
        let log_path = self.next_log_path();
        let log = File::create(&log_path)?;
        let mut command = Command::new(&spec.command);
        command.args(&spec.args).stdin(Stdio::null());
        if let Some(output) = &self.output {
            command
                .stdout(Stdio::from(output.try_clone_writer()?))
                .stderr(Stdio::from(output.try_clone_writer()?));
        } else {
            command
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log));
        }
        command.envs(spec.env.iter().cloned());
        command.env_remove("CLIPD_TOKEN");
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

/// A deliberately small single-pipe collector. T4.3 can replace the reader's
/// closing/limiting policy without changing Runner or the Rhai callbacks.
#[derive(Clone)]
pub(crate) struct OrderedOutput {
    writer: Arc<Mutex<File>>,
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl OrderedOutput {
    pub(crate) fn new() -> io::Result<(Self, thread::JoinHandle<io::Result<()>>)> {
        let (mut reader, writer) = pipe()?;
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let reader_bytes = Arc::clone(&bytes);
        let relay = thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    return Ok(());
                }
                reader_bytes
                    .lock()
                    .unwrap()
                    .extend_from_slice(&buffer[..count]);
            }
        });
        Ok((
            Self {
                writer: Arc::new(Mutex::new(writer)),
                bytes,
            },
            relay,
        ))
    }

    pub(crate) fn write_line(&self, text: &str) {
        let mut writer = self.writer.lock().unwrap();
        let _ = writer.write_all(text.as_bytes());
        let _ = writer.write_all(b"\n");
    }

    pub(crate) fn write_all(&self, bytes: &[u8]) {
        let _ = self.writer.lock().unwrap().write_all(bytes);
    }

    fn try_clone_writer(&self) -> io::Result<File> {
        self.writer.lock().unwrap().try_clone()
    }

    pub(crate) fn finish(self, relay: thread::JoinHandle<io::Result<()>>) -> io::Result<Vec<u8>> {
        let bytes = Arc::clone(&self.bytes);
        drop(self);
        relay
            .join()
            .map_err(|_| io::Error::other("output relay panicked"))??;
        let result = bytes.lock().unwrap().clone();
        Ok(result)
    }
}

#[cfg(not(windows))]
fn pipe() -> io::Result<(File, File)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    // SAFETY: pipe initializes both integers on success; each descriptor is
    // transferred exactly once into an owning File.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

#[cfg(windows)]
fn pipe() -> io::Result<(File, File)> {
    use ::windows::Win32::{Foundation::HANDLE, System::Pipes::CreatePipe};
    use std::os::windows::io::FromRawHandle;
    let mut reader = HANDLE::default();
    let mut writer = HANDLE::default();
    // SAFETY: CreatePipe initializes both handles and ownership is immediately
    // transferred to File. Command's Stdio machinery duplicates child handles.
    unsafe { CreatePipe(&mut reader, &mut writer, None, 0) }
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(unsafe {
        (
            File::from_raw_handle(reader.0),
            File::from_raw_handle(writer.0),
        )
    })
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

    pub(crate) fn wait_cancelable(&mut self, cancelled: &AtomicBool) -> io::Result<JobState> {
        let Some(child) = &mut self.child else {
            return Ok(self.state);
        };
        loop {
            if let Some(status) = child.try_wait()? {
                self.record_status(status);
                return Ok(self.state);
            }
            if cancelled.load(Ordering::Relaxed) {
                self.terminate()?;
                return self.wait();
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn terminate(&mut self) -> io::Result<()> {
        #[cfg(not(windows))]
        return self.group.terminate();
        #[cfg(windows)]
        {
            // T4.4 owns Job Objects. Until then Windows deliberately retains
            // the old std::process fallback and can only kill the direct child.
            if let Some(child) = &mut self.child {
                child.kill()
            } else {
                Ok(())
            }
        }
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
