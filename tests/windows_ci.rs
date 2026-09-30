//! Windows でしか成立しない性質の足場(T0.5)。Linux では空のテストバイナリになる。
#![cfg(windows)]

use std::{
    fs,
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// AC-T0.5.2: ファイルを開いたままの別プロセスがあると remove_dir_all は失敗し、
/// 閉じると成功する。
#[test]
fn ac_t0_5_2_remove_dir_all_fails_while_another_process_holds_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("job");
    fs::create_dir(&root).unwrap();
    let file = root.join("held.log");
    fs::write(&file, b"x").unwrap();

    // FileShare.Read のみ: 他者の削除・書き込みを許さない。
    let script = format!(
        "$f=[IO.File]::Open('{}','Open','Read','Read'); [Console]::Out.WriteLine('ready'); [Console]::Out.Flush(); Start-Sleep -Seconds 60",
        file.display()
    );
    let mut child = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn powershell");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "ready");

    assert!(
        fs::remove_dir_all(&root).is_err(),
        "remove_dir_all must fail while the file is held open"
    );

    child.kill().unwrap();
    child.wait().unwrap();

    // ハンドル解放の反映を短く待つ。
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match fs::remove_dir_all(&root) {
            Ok(()) => break,
            Err(error) if Instant::now() >= deadline => {
                panic!("remove_dir_all still failing after the holder exited: {error}")
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
    assert!(!root.exists());
}
