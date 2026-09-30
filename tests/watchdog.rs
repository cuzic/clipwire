//! T0.4 ウォッチドッグの結合テスト。実バイナリのコピーを「good」として、
//! サーバ不在 → 自動復旧、保守ファイル中は復旧しない、を実プロセスで確認する。
//! Linux / Windows の両 CI で動く (Windows 固有の起動フラグ・ミューテックスも通る)。

use std::{
    fs,
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn healthy(port: u16) -> bool {
    matches!(
        ureq::get(&format!("http://127.0.0.1:{port}/health"))
            .timeout(Duration::from_millis(500))
            .call(),
        Ok(r) if r.status() == 200
    )
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(200));
    }
    cond()
}

/// ウォッチドッグ本体と、それが起動した good を後始末する。
struct Harness {
    watchdog: Child,
    good: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.watchdog.kill();
        let _ = self.watchdog.wait();
        // good は切り離されて起動されるので、コピーの一意なパスで止める。
        #[cfg(windows)]
        let _ = Command::new("taskkill")
            .args(["/F", "/IM"])
            .arg(self.good.file_name().unwrap())
            .output();
        #[cfg(not(windows))]
        let _ = Command::new("pkill").arg("-f").arg(&self.good).output();
    }
}

fn start_watchdog(port: u16, dir: tempfile::TempDir, maintenance: Option<&Path>) -> Harness {
    let exe = if cfg!(windows) {
        format!("clipwire-wdtest-{port}.exe")
    } else {
        format!("clipwire-wdtest-{port}")
    };
    let good = dir.path().join(&exe);
    fs::copy(env!("CARGO_BIN_EXE_clipwire"), &good).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_clipwire"));
    cmd.args(["watchdog", "--good"]).arg(&good).args([
        "--port",
        &port.to_string(),
        "--interval",
        "1",
    ]);
    if let Some(m) = maintenance {
        cmd.arg("--maintenance-file").arg(m);
    }
    cmd.args(["--", "--bind-localhost-only", "--allow-no-token"])
        .env("CLIPWIRE_CONFIG_DIR", dir.path())
        .env_remove("CLIPD_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let watchdog = cmd.spawn().expect("spawn watchdog");
    Harness {
        watchdog,
        good,
        _dir: dir,
    }
}

/// AC-T0.4.1 (C): サーバが無い状態から、手作業なしで good により復旧して /health が 200。
#[test]
fn ac_t0_4_1_watchdog_recovers_a_missing_server_with_good() {
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let mut h = start_watchdog(port, dir, None);
    assert!(
        wait_until(Duration::from_secs(40), || healthy(port)),
        "server was not recovered; watchdog exited: {:?}",
        h.watchdog.try_wait()
    );
    // 復旧後にウォッチドッグ自身は生きている
    assert!(h.watchdog.try_wait().unwrap().is_none());
}

/// AC-T0.4.2 (C): 保守ファイルが有効な間は復旧しない。削除すると復旧する。
/// 期限切れのファイルは無視される。
#[test]
fn ac_t0_4_2_maintenance_file_suppresses_recovery_until_removed_or_expired() {
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let maintenance = dir.path().join("maintenance");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::write(&maintenance, (now + 3600).to_string()).unwrap();
    let h = start_watchdog(port, dir, Some(&maintenance));

    // 3 回失敗 + 余裕ぶんより長く待っても、復旧しない
    assert!(
        !wait_until(Duration::from_secs(8), || healthy(port)),
        "recovered during maintenance"
    );
    fs::remove_file(&maintenance).unwrap();
    assert!(
        wait_until(Duration::from_secs(40), || healthy(port)),
        "not recovered after maintenance ended"
    );
    drop(h);

    // 期限切れ(過去)のファイルは無視され、復旧する
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let maintenance = dir.path().join("maintenance");
    fs::write(&maintenance, (now - 10).to_string()).unwrap();
    let _h = start_watchdog(port, dir, Some(&maintenance));
    assert!(
        wait_until(Duration::from_secs(40), || healthy(port)),
        "expired maintenance file must be ignored"
    );
}

/// AC-T0.4.4 (C): good が起動できなくても、ウォッチドッグは落ちずに試行を続ける。
#[test]
fn ac_t0_4_4_watchdog_survives_an_unstartable_good() {
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_clipwire"))
        .args(["watchdog", "--good"])
        .arg(dir.path().join("does-not-exist"))
        .args(["--port", &port.to_string(), "--interval", "1"])
        .env("CLIPWIRE_CONFIG_DIR", dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_secs(7));
    let alive = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(alive, "watchdog must keep running when good cannot start");
    let log = fs::read_to_string(dir.path().join("watchdog.log")).unwrap_or_default();
    assert!(log.contains("good の起動に失敗"), "log: {log}");
}
