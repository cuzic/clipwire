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
        let mut child = Command::new(python)
            .arg(wrapper_path)
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

    /// Drops the child's stdin handle, delivering EOF on its read end —
    /// simulates the server process (and so the pipe) going away without
    /// killing the wrapper directly, to test that the wrapper notices on
    /// its own (ADR-0013 §3's EOF requirement).
    fn close_stdin(&mut self) {
        self.stdin = None;
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
/// sleep で代用)でも、stdin への EOF(サーバプロセスの終了を模する)を
/// 別スレッドで検知して速やかに終了すること。当初の実装は stdin の読み
/// 取りをメインスレッド(ターン実行スレッドそのもの)で行っていたため、
/// ハング中のターンは EOF に気づけず、サーバ再起動後も `JobRegistry` の
/// 「席」を握り続けてしまう(ADR-0013 §3 改訂3 で要件化)。
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
    // native call (ADR-0013 Status 2's remaining, narrower limitation).
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
