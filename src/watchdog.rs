//! ウォッチドッグ (T0.4): `/health` を定期確認し、連続して失敗したときだけ
//! 動作確認済みの `good` バイナリで `serve` を起動し直す。
//!
//! 判断ロジック (`WatchState::step`, `maintenance_active`) は副作用を持たず、
//! Linux の単体テストで検証できる。プロセス起動と単一起動の排他だけが
//! Windows 固有。

use super::*;

/// 連続何回の失敗で復旧を始めるか。
pub(crate) const FAILS_BEFORE_RECOVERY: u32 = 3;
/// 復旧の起動に失敗し続けたときのバックオフ上限 (tick 数)。
const MAX_BACKOFF_TICKS: u32 = 20;

#[derive(Args, Debug)]
pub(crate) struct WatchdogArgs {
    /// 復旧に使う動作確認済みバイナリのパス (bin\good 以下に置く)
    #[arg(long, value_name = "PATH")]
    pub(crate) good: PathBuf,

    /// 監視する serve のポート
    #[arg(long, default_value = "9999")]
    pub(crate) port: u16,

    /// 確認間隔 (秒)
    #[arg(long, default_value = "30")]
    pub(crate) interval: u64,

    /// 保守ファイル。有効期限(UNIX 秒)が未来の間は復旧しない
    /// (既定: <設定ディレクトリ>/maintenance)
    #[arg(long, value_name = "PATH")]
    pub(crate) maintenance_file: Option<PathBuf>,

    /// 1 回だけ判断して終了する (動作確認用)
    #[arg(long)]
    pub(crate) once: bool,

    /// good に渡す serve の引数 (`--` の後ろ。例: -- --auto-approve --token-file T)
    #[arg(last = true)]
    pub(crate) serve_args: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Nothing,
    Recover,
}

#[derive(Debug, Default)]
pub(crate) struct WatchState {
    fails: u32,
    spawn_failures: u32,
    cooldown: u32,
}

impl WatchState {
    /// 1 tick ぶんの判断。`Recover` を返したら、呼び出し側は起動を試み、
    /// 結果を `spawned` で報告する。
    pub(crate) fn step(&mut self, health_ok: bool, maintenance: bool) -> Action {
        if health_ok {
            *self = Self::default();
            return Action::Nothing;
        }
        if maintenance {
            // 更新中の停止は失敗に数えない (期限が切れた後に即復旧しないため)
            self.fails = 0;
            return Action::Nothing;
        }
        if self.cooldown > 0 {
            // 起動直後・バックオフ中は失敗に数えない
            self.cooldown -= 1;
            return Action::Nothing;
        }
        self.fails += 1;
        if self.fails >= FAILS_BEFORE_RECOVERY {
            Action::Recover
        } else {
            Action::Nothing
        }
    }

    /// `Recover` の結果を報告する。失敗が続くと待ち tick 数が倍々に増える。
    pub(crate) fn spawned(&mut self, ok: bool) {
        if ok {
            // 起動直後は /health がまだ応答しないので、数 tick は様子を見る
            self.spawn_failures = 0;
            self.fails = 0;
            self.cooldown = 2;
        } else {
            self.spawn_failures += 1;
            self.cooldown = (1u32 << self.spawn_failures.min(5)).min(MAX_BACKOFF_TICKS);
        }
    }
}

/// 保守ファイルが有効か。中身は有効期限の UNIX 秒。読めない・壊れている・
/// 期限切れなら無効 (= 復旧を妨げない)。
pub(crate) fn maintenance_active(path: &Path, now_unix: u64) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .is_some_and(|expiry| expiry > now_unix)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn health_ok(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/health");
    matches!(
        ureq::get(&url).timeout(Duration::from_secs(5)).call(),
        Ok(r) if r.status() == 200
    )
}

/// `good serve <args>` を切り離して起動する。Job の BREAKAWAY が許されて
/// いなければ、フラグなしで再試行する (どちらだったかをログに残す。AC-T0.4.7)。
fn spawn_good(args: &WatchdogArgs) -> bool {
    use std::process::{Command, Stdio};
    let build = || {
        let mut cmd = Command::new(&args.good);
        cmd.arg("serve")
            .arg("--port")
            .arg(args.port.to_string())
            .args(&args.serve_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let base = CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS;
        match build()
            .creation_flags(base | CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
        {
            Ok(child) => {
                info!("watchdog: good を起動 (pid={}, breakaway=許可)", child.id());
                return true;
            }
            Err(e) => warn!("watchdog: BREAKAWAY 付きの起動に失敗 ({e})。なしで再試行"),
        }
        match build().creation_flags(base).spawn() {
            Ok(child) => {
                warn!("watchdog: good を起動 (pid={}, breakaway=なし)", child.id());
                true
            }
            Err(e) => {
                warn!("watchdog: good の起動に失敗: {e}");
                false
            }
        }
    }
    #[cfg(not(windows))]
    match build().spawn() {
        Ok(child) => {
            info!("watchdog: good を起動 (pid={})", child.id());
            true
        }
        Err(e) => {
            warn!("watchdog: good の起動に失敗: {e}");
            false
        }
    }
}

pub(crate) fn run_watchdog(args: WatchdogArgs) -> Result<()> {
    let config_dir = clipwire_config_dir();
    let _ = std::fs::create_dir_all(&config_dir);
    let log = tracing_appender::rolling::never(&config_dir, "watchdog.log");
    let (writer, _guard) = tracing_appender::non_blocking(log);
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .init();

    #[cfg(windows)]
    let _mutex = unsafe {
        win_clip::acquire_named_mutex(
            &singleton_mutex_name(&config_dir.join("watchdog")),
            "clipwire watchdog は既に起動中です。",
        )
    };

    let maintenance = args
        .maintenance_file
        .clone()
        .unwrap_or_else(|| config_dir.join("maintenance"));
    info!(
        "watchdog starting (pid={}, good={}, interval={}s)",
        std::process::id(),
        args.good.display(),
        args.interval
    );
    let mut state = WatchState::default();
    loop {
        let ok = health_ok(args.port);
        let maint = maintenance_active(&maintenance, now_unix());
        if !ok {
            warn!("watchdog: /health 失敗 (保守中={maint})");
        }
        if state.step(ok, maint) == Action::Recover {
            warn!("watchdog: 連続失敗のため復旧を開始");
            state.spawned(spawn_good(&args));
        }
        if args.once {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(args.interval));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_only_after_three_consecutive_failures() {
        let mut s = WatchState::default();
        assert_eq!(s.step(false, false), Action::Nothing);
        assert_eq!(s.step(false, false), Action::Nothing);
        assert_eq!(s.step(false, false), Action::Recover);
    }

    #[test]
    fn a_healthy_tick_resets_the_count() {
        let mut s = WatchState::default();
        s.step(false, false);
        s.step(false, false);
        s.step(true, false);
        assert_eq!(s.step(false, false), Action::Nothing);
        assert_eq!(s.step(false, false), Action::Nothing);
        assert_eq!(s.step(false, false), Action::Recover);
    }

    #[test]
    fn maintenance_suppresses_recovery_and_does_not_count() {
        let mut s = WatchState::default();
        for _ in 0..10 {
            assert_eq!(s.step(false, true), Action::Nothing);
        }
        // 保守が終わっても、そこから 3 回数え直す
        assert_eq!(s.step(false, false), Action::Nothing);
        assert_eq!(s.step(false, false), Action::Nothing);
        assert_eq!(s.step(false, false), Action::Recover);
    }

    #[test]
    fn spawn_failure_backs_off_with_a_cap() {
        let mut s = WatchState::default();
        let mut gaps = Vec::new();
        for _ in 0..8 {
            let mut ticks = 0;
            while s.step(false, false) != Action::Recover {
                ticks += 1;
                assert!(ticks < 1000);
            }
            gaps.push(ticks);
            s.spawned(false);
        }
        assert!(gaps.windows(2).all(|w| w[1] >= w[0]));
        assert!(*gaps.last().unwrap() <= MAX_BACKOFF_TICKS + FAILS_BEFORE_RECOVERY);
    }

    #[test]
    fn successful_spawn_waits_for_startup_before_judging_again() {
        let mut s = WatchState::default();
        for _ in 0..3 {
            s.step(false, false);
        }
        s.spawned(true);
        // 様子見 2 tick + 数え直し 2 tick は Nothing、3 回目の失敗で Recover
        for _ in 0..4 {
            assert_eq!(s.step(false, false), Action::Nothing);
        }
        assert_eq!(s.step(false, false), Action::Recover);
    }

    #[test]
    fn maintenance_file_honours_expiry_and_ignores_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("maintenance");
        assert!(!maintenance_active(&p, 100));
        std::fs::write(&p, "200\n").unwrap();
        assert!(maintenance_active(&p, 100));
        assert!(!maintenance_active(&p, 200));
        std::fs::write(&p, "not a number").unwrap();
        assert!(!maintenance_active(&p, 100));
    }
}
