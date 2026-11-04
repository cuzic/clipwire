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
    stdin: ChildStdin,
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
            stdin,
            responses: rx,
        }
    }

    fn turn(&mut self, code: &str) -> serde_json::Value {
        let request = serde_json::json!({ "code": code });
        let line = serde_json::to_string(&request).unwrap();
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
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
