//! T9.3 (ADR-0013) 永続実行コンテキストの PoC。本実装の前段として、
//! ADR-0013 末尾の PoC 項目 1・2 を実プロセスで検証する:
//!   1. ラッパー内の stdout/stderr 捕捉が、print()・サブプロセス継承・
//!      生の fd 書き込みを漏れなく拾えるか。
//!   2. Stdio::piped() 越しの改行区切り JSON フレーミングが、大きい
//!      出力・高頻度のターンでも壊れないか。
//!
//! ラッパー試作は docs/tasks/evidence/poc/t9.3/wrapper.py。結果は
//! docs/tasks/evidence/t9.3-persistent-context-poc.md に記録する。
//! `PersistentContextRegistry` 等の本実装はまだ存在しない
//! (ADR-0013 は PoC 完了まで Proposed のまま)。

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    thread,
};

fn find_python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|candidate| {
        Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

struct Wrapper {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: std::sync::mpsc::Receiver<String>,
}

impl Wrapper {
    fn spawn(python: &str) -> Self {
        let wrapper_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/tasks/evidence/poc/t9.3/wrapper.py"
        );
        let mut cmd = Command::new(python);
        cmd.arg(wrapper_path);
        Self::spawn_with_command(cmd)
    }

    /// Spawns wrapper.py behind an intermediate Python "stub" process that
    /// does nothing but launch it and wait, inheriting this process's
    /// piped stdin/stdout/stderr through to wrapper.py unchanged. This
    /// makes the stub (not this test process) wrapper.py's real OS-level
    /// parent, so a test can kill the stub — simulating "the real server
    /// process died" — while this test process keeps running to observe
    /// the result. That isn't possible when this test process is
    /// wrapper.py's direct parent, since killing it would end the test
    /// itself.
    fn spawn_behind_stub(python: &str) -> Self {
        let wrapper_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/tasks/evidence/poc/t9.3/wrapper.py"
        );
        let mut cmd = Command::new(python);
        cmd.arg("-c").arg(format!(
            "import subprocess, sys; p = subprocess.Popen([sys.executable, {wrapper_path:?}]); p.wait()"
        ));
        Self::spawn_with_command(cmd)
    }

    fn spawn_with_command(mut cmd: Command) -> Self {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wrapper.py");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        // ADR-0013 §2: ラッパー自身の stderr は継続的に排出する(排出せず
        // パイプが埋まるとターンがハングする)。ここでは drain のみ行う。
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("[wrapper stderr] {line}");
            }
        });

        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        Self {
            child,
            stdin: Some(stdin),
            responses: rx,
        }
    }

    fn send_code(&mut self, code: &str) {
        let request = serde_json::json!({ "code": code });
        let line = serde_json::to_string(&request).unwrap();
        let stdin = self.stdin.as_mut().expect("stdin not closed");
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    /// Drops this test process's handle to wrapper.py's stdin pipe,
    /// delivering EOF on its read end. Since only the process holding the
    /// pipe's *write* end matters for EOF (not however many processes sit
    /// between it and wrapper.py on the read side), this correctly
    /// simulates "the server died" regardless of whether wrapper.py was
    /// spawned directly or behind a relay (`spawn_behind_stub`).
    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn turn(&mut self, code: &str) -> serde_json::Value {
        self.send_code(code);
        let response = self
            .responses
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("wrapper response within 10s");
        serde_json::from_str(&response).unwrap_or_else(|error| {
            panic!("response was not a single JSON line: {error}: {response:?}")
        })
    }
}

impl Drop for Wrapper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Polls for `path` to exist, or panics after `timeout`. Used to
/// synchronize on something that happens *during* a turn (e.g. "I am now
/// inside the sleep", or "the background thread's write already
/// happened") without guessing with a fixed delay, which is flaky under
/// CI load. A turn's own stdout/stderr are captured into its JSON
/// response rather than relayed live, so a file is the simplest way for
/// code running inside a turn to signal the test process.
fn wait_for_file(path: &std::path::Path, timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{path:?} did not appear within {timeout:?}"
        );
        thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// PoC 項目 1: print()・サブプロセス継承・生の fd 書き込みのすべてが
/// 同じターンのレスポンスに漏れなく含まれるか。
#[test]
fn poc_item1_stdout_capture_covers_subprocess_and_raw_fd_writes() {
    let Some(python) = find_python() else {
        eprintln!("skipping: no python3/python interpreter on PATH");
        return;
    };
    let mut wrapper = Wrapper::spawn(python);

    let code = r#"
import os, subprocess, sys
print("from_print")
subprocess.run([sys.executable, "-c", "print('from_subprocess')"], check=True)
os.write(1, b"from_raw_fd\n")
print("from_stderr", file=sys.stderr)
"#;
    let response = wrapper.turn(code);
    let stdout = response["stdout"].as_str().unwrap_or_default();
    let stderr = response["stderr"].as_str().unwrap_or_default();
    assert!(response["error"].is_null(), "unexpected error: {response}");

    assert!(stdout.contains("from_print"), "stdout={stdout:?}");
    assert!(
        stdout.contains("from_subprocess"),
        "subprocess output (inherited fd) missing from capture: stdout={stdout:?}"
    );
    assert!(
        stdout.contains("from_raw_fd"),
        "raw os.write(1, ..) output missing from capture: stdout={stdout:?}"
    );
    assert!(stderr.contains("from_stderr"), "stderr={stderr:?}");
}

/// レビューで判明した不備の回帰テスト: ターンがハング中(ここでは長い
/// sleep で代用)でも、stdin の EOF(サーバプロセスの終了を模する)を
/// 別スレッドで検知して速やかに終了すること。
///
/// この検知方式は改訂6で一度「親 PID の生存ポーリング」に置き換えられた
/// (別スレッドが stdin をブロッキング読み取りしている状態でターン内から
/// subprocess を起動すると Windows 実機 CI だけで必ずハングすると判明した
///ため)。しかし `script_file` の `interpreter` が venv のリダイレクタ・
/// `uv run`・py ランチャーのような中継プロセスを経由する構成では、
/// ラッパーの実の親はサーバではなく中継プロセスになり、サーバが死んでも
/// 中継プロセスは死なないため `getppid()` ベースの監視が機能しないと
/// 2回目のレビューで指摘された。
///
/// 改訂7で、ハングの原因は「stdin のパイプ自体」ではなく「fd 0 という
/// **番号**を、別スレッドがブロッキング読み取り中に、別のスレッドで
/// `subprocess` を起動すると(stdin 未指定時に CPython が fd 0 に触れる
/// ため)Windows で衝突する」ことだと特定した。fd 0 の番号自体を恒久的に
/// devnull へ向け、実際のプロトコル入力は fd 0 とは独立した
/// non-inheritable な fd から読むようにしたところハングが解消したため、
/// 本来の stdin EOF 方式に戻した。EOF はパイプの**書き込み側**を持つ
/// プロセス(サーバ)が死ねば、中継プロセスが何段挟まっていても確実に
/// 起きるため、中継プロセスの問題も同時に解決する(次のテスト参照)。
#[test]
fn poc_eof_during_a_hung_turn_still_exits_promptly() {
    let Some(python) = find_python() else {
        eprintln!("skipping: no python3/python interpreter on PATH");
        return;
    };
    let mut wrapper = Wrapper::spawn(python);
    let marker = std::env::temp_dir().join(format!("t93-entering-sleep-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);

    // Starts a turn that would otherwise hang for 30s (standing in for a
    // pywinauto call stuck on a modal dialog). Don't wait for its response.
    // `time.sleep` releases the GIL, so the reader thread can still run
    // during it — this test covers exactly that case, not a GIL-holding
    // native call (ADR-0013 Consequences' remaining, narrower limitation).
    // A turn's own stdout/stderr are captured into its eventual JSON
    // response rather than relayed live, so a marker file (not a print) is
    // how the turn signals "I am now inside the sleep" to the test.
    wrapper.send_code(&format!(
        "open(r'{}', 'w').close()\nimport time; time.sleep(30)",
        marker.display()
    ));
    // Wait for proof the turn actually reached the sleep (is genuinely
    // hung) before simulating the server's death, instead of guessing with
    // a fixed delay.
    wait_for_file(&marker, std::time::Duration::from_secs(5));
    let _ = std::fs::remove_file(&marker);
    wrapper.close_stdin();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(Some(_status)) = wrapper.child.try_wait() {
            return; // exited on its own well before the 30s sleep would finish.
        }
        assert!(
            std::time::Instant::now() < deadline,
            "wrapper did not exit within 5s of stdin EOF despite a hung turn"
        );
        thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// レビューで判明した不備の回帰テスト: `script_file` の `interpreter` が
/// venv のリダイレクタ・`uv run`・py ランチャーのような中継プロセスを
/// 経由していても、サーバが死んだ(= このテストプロセスが stdin の
/// パイプを閉じた)ことを検知できること。中継プロセス(スタブ)自体は
/// **生かしたまま**にする——EOF はパイプの書き込み側を持つプロセスにしか
/// 依存しないため、中継プロセスが生きていても検知できるはずだが、
/// `getppid()` ベースの監視(改訂6で一度採用し、改訂7で撤回)では中継
/// プロセスの生存が検知を妨げてしまっていた。
#[test]
fn poc_eof_propagates_through_an_intermediate_relay_process() {
    let Some(python) = find_python() else {
        eprintln!("skipping: no python3/python interpreter on PATH");
        return;
    };
    let mut wrapper = Wrapper::spawn_behind_stub(python);
    let r1 = wrapper.turn("print('alive')");
    assert_eq!(r1["stdout"].as_str().unwrap().trim(), "alive");

    // Close only this test process's handle to the pipe. The stub (relay)
    // process is deliberately left running — it still holds its own
    // inherited handle to the same pipe, so if EOF depended on relay-side
    // closure too, this would never complete.
    wrapper.close_stdin();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Err(std::sync::mpsc::RecvTimeoutError::Disconnected) = wrapper
            .responses
            .recv_timeout(std::time::Duration::from_millis(50))
        {
            return; // wrapper.py exited on its own, independent of the relay.
        }
        assert!(
            std::time::Instant::now() < deadline,
            "wrapper did not exit within 5s of stdin EOF despite a live relay process"
        );
    }
}

/// レビューで判明した不備の回帰テスト: ターンの**外**(メインスレッドが
/// 次のターンを待って stdin を読んでいる間)で、以前のターンが残した
/// バックグラウンドスレッドが `subprocess` を起動しても、次のターンの
/// 応答が(ハングせず)速やかに返ること。切り分けで特定した「別スレッドが
/// stdin をブロッキング読み取り中に、別のスレッドから `subprocess` を
/// 起動する」という条件は、ターンの中(メインスレッドがターン実行中)
/// だけでなく、ターンの外(メインスレッドが次の行を待っている間)でも
/// 同じ組み合わせになりうる、という指摘を受けて追加。
#[test]
fn poc_subprocess_from_background_thread_between_turns_does_not_hang() {
    let Some(python) = find_python() else {
        eprintln!("skipping: no python3/python interpreter on PATH");
        return;
    };
    let mut wrapper = Wrapper::spawn(python);

    // Turn 1 starts a background thread and returns immediately — the
    // thread's subprocess call happens *after* this turn's response is
    // already on the wire, i.e. while the main thread is back to blocking
    // on the next line of stdin.
    let code = r#"
import subprocess, sys, threading
def _late_subprocess():
    subprocess.run([sys.executable, "-c", "print('from_background_subprocess')"], check=True)
threading.Thread(target=_late_subprocess, daemon=True).start()
print("turn1-done")
"#;
    let r1 = wrapper.turn(code);
    assert_eq!(r1["stdout"].as_str().unwrap().trim(), "turn1-done");

    // If the background thread's subprocess call blocks the stdin reader
    // thread (or vice versa), this next turn would hang; `turn()` already
    // enforces a 10s bound via its own recv_timeout.
    let r2 = wrapper.turn("print('turn2-done')");
    assert_eq!(r2["stdout"].as_str().unwrap().trim(), "turn2-done");
}

/// レビューで判明した不備の回帰テスト: ターン中に起動したバックグラウン
/// ドスレッドが、そのターンの応答が返った**後**に fd 1 へ書いても、次の
/// ターンの応答のフレーミングを壊さないこと。当初の wrapper.py 実装は
/// ターン終了時に fd 1 をプロトコル用パイプへ戻していたため、この書き込
/// みが直接プロトコルに混入していた(ADR-0013 §2 改訂3 時点では未検出)。
#[test]
fn poc_item1_late_write_after_turn_does_not_corrupt_next_turns_framing() {
    let Some(python) = find_python() else {
        eprintln!("skipping: no python3/python interpreter on PATH");
        return;
    };
    let mut wrapper = Wrapper::spawn(python);
    let go_marker = std::env::temp_dir().join(format!("t93-late-write-go-{}", std::process::id()));
    let done_marker =
        std::env::temp_dir().join(format!("t93-late-write-done-{}", std::process::id()));
    let _ = std::fs::remove_file(&go_marker);
    let _ = std::fs::remove_file(&done_marker);

    // The background thread must not write until *after* the test has
    // actually received turn 1's response (proof that run_turn's fd 1/2
    // restore to the log side has already happened) — otherwise, with no
    // delay at all the write can race into turn 1's own capture window
    // (which the previous, sleep-based version of this test papered over
    // with a guessed delay). `go_marker` makes that ordering exact instead
    // of guessed.
    let code = format!(
        r#"
import os, threading, time
def _late():
    while not os.path.exists(r"{go}"):
        time.sleep(0.01)
    os.write(1, b"late-background-write\n")
    open(r"{done}", "w").close()
threading.Thread(target=_late, daemon=True).start()
print("turn1-done")
"#,
        go = go_marker.display(),
        done = done_marker.display(),
    );
    let r1 = wrapper.turn(&code);
    assert_eq!(r1["stdout"].as_str().unwrap().trim(), "turn1-done");

    std::fs::File::create(&go_marker).unwrap();
    wait_for_file(&done_marker, std::time::Duration::from_secs(5));
    let _ = std::fs::remove_file(&go_marker);
    let _ = std::fs::remove_file(&done_marker);

    let r2 = wrapper.turn("print('turn2-done')");
    assert_eq!(
        r2["stdout"].as_str().unwrap().trim(),
        "turn2-done",
        "a late write from turn 1's background thread corrupted turn 2's response: {r2}"
    );
}

/// PoC 項目 2: 大きい出力・複数ターンにわたって、改行区切り JSON の
/// フレーミングが崩れない(行の分割・結合が起きない)か。ターン間で
/// 状態(globals)が引き継がれることも併せて確認する(永続コンテキスト
/// の前提)。
#[test]
fn poc_item2_newline_framing_survives_large_output_and_many_turns() {
    let Some(python) = find_python() else {
        eprintln!("skipping: no python3/python interpreter on PATH");
        return;
    };
    let mut wrapper = Wrapper::spawn(python);

    // 1. 状態がターンを跨いで残ることの確認。
    let r1 = wrapper.turn("counter = 0\nprint(counter)");
    assert_eq!(r1["stdout"].as_str().unwrap().trim(), "0");
    for expected in 1..=5 {
        let r = wrapper.turn("counter += 1\nprint(counter)");
        assert_eq!(r["stdout"].as_str().unwrap().trim(), expected.to_string());
    }

    // 2. 1 ターンで 200KB 超(パイプの既定バッファを超える)の出力を
    //    送っても、1 回の recv が壊れた/分割された JSON にならない。
    let big = wrapper.turn("print('x' * 200_000)");
    let stdout = big["stdout"].as_str().unwrap();
    assert_eq!(stdout.trim_end().len(), 200_000);

    // 3. 小さいターンを高頻度に送って、行の境界がずれない
    //    (前のターンの末尾と次のターンの先頭が混ざらない)ことを確認。
    for i in 0..50 {
        let r = wrapper.turn(&format!("print('turn-{i}')"));
        assert_eq!(r["stdout"].as_str().unwrap().trim(), format!("turn-{i}"));
    }
}
