use std::{
    io,
    net::{Ipv4Addr, TcpListener},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Mutex, MutexGuard, OnceLock},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

static STARTUP_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub struct TestServer {
    child: Child,
    config_dir: TempDir,
    port: u16,
}

impl TestServer {
    pub fn start() -> Option<Self> {
        // Hold the allocation lock until the child has bound its port. This
        // closes the usual bind(0)-then-spawn race between parallel tests.
        let _startup = startup_lock();
        let port = match available_port() {
            Ok(port) => port,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping server integration test: loopback bind is forbidden: {error}");
                return None;
            }
            Err(error) => panic!("allocate test server port: {error}"),
        };
        let config_dir = tempfile::tempdir().expect("create test config directory");
        let child = Command::new(env!("CARGO_BIN_EXE_clipwire"))
            .args([
                "serve",
                "--bind-localhost-only",
                "--port",
                &port.to_string(),
            ])
            .env("CLIPWIRE_CONFIG_DIR", config_dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn clipwire test server");

        let mut server = Self {
            child,
            config_dir,
            port,
        };
        server.wait_until_ready();
        Some(server)
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}:{}{path}", Ipv4Addr::LOCALHOST, self.port)
    }

    pub fn config_dir(&self) -> &Path {
        self.config_dir.path()
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn wait_until_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().expect("inspect test server") {
                panic!("clipwire test server exited before readiness: {status}");
            }
            match ureq::get(&self.url("/health"))
                .timeout(Duration::from_millis(200))
                .call()
            {
                Ok(response) if response.status() == 200 => return,
                _ if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
                _ => panic!(
                    "clipwire test server did not become ready on port {}",
                    self.port
                ),
            }
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn startup_lock() -> MutexGuard<'static, ()> {
    STARTUP_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn available_port() -> io::Result<u16> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
}
