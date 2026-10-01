//! Windows でしか成立しない性質の足場(T0.5)。Linux では空のテストバイナリになる。
#![cfg(windows)]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start_server(config_dir: &Path, port: u16, auto_approve: bool) -> Server {
    let mut args = vec![
        "serve",
        "--bind-localhost-only",
        "--allow-no-token",
        "--port",
    ];
    let port_text = port.to_string();
    args.push(&port_text);
    if auto_approve {
        args.push("--auto-approve");
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_clipwire"))
        .args(args)
        .env("CLIPWIRE_CONFIG_DIR", config_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ureq::get(&format!("http://127.0.0.1:{port}/health"))
            .call()
            .is_ok()
        {
            return Server(child);
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("server on port {port} did not start");
}

fn register_range(port: u16, prefix: &str) {
    for index in 0..100 {
        let body = serde_json::json!({
            "name": format!("{prefix}-{index}"),
            "target": {"script": format!("echo {prefix}-{index}")}
        });
        ureq::post(&format!("http://127.0.0.1:{port}/register"))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .unwrap();
    }
}

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

/// AC-T3.3.5 (C): a server and local CLI processes share the named mutex;
/// their read-modify-write cycles retain the union of both writers.
#[test]
fn ac_t3_3_5_server_and_cli_processes_preserve_the_union() {
    let dir = tempfile::tempdir().unwrap();
    let mut pending = String::new();
    for index in 0..100 {
        pending.push_str(&format!("[cli-{index}]\nscript = \"echo cli-{index}\"\n"));
    }
    fs::write(dir.path().join("pending.toml"), pending).unwrap();

    // approve は --hash 必須で y/N を尋ねるので、先に pending からハッシュを集める。
    let listing = Command::new(env!("CARGO_BIN_EXE_clipwire"))
        .arg("pending")
        .env("CLIPWIRE_CONFIG_DIR", dir.path())
        .output()
        .unwrap();
    assert!(listing.status.success());
    let hashes: Vec<(String, String)> = String::from_utf8(listing.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let (hash, name) = line.split_once("  ")?;
            Some((name.to_owned(), hash.to_owned()))
        })
        .collect();
    assert_eq!(hashes.len(), 100);

    let port = free_port();
    let _server = start_server(dir.path(), port, true);
    let server_writes = thread::spawn(move || register_range(port, "server"));
    let config_dir = dir.path().to_owned();
    let cli_writes = thread::spawn(move || {
        for (name, hash) in hashes {
            let mut child = Command::new(env!("CARGO_BIN_EXE_clipwire"))
                .args(["approve", &name, "--hash", &hash])
                .env("CLIPWIRE_CONFIG_DIR", &config_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(b"y\n").unwrap();
            assert!(child.wait().unwrap().success(), "approve {name} failed");
        }
    });
    server_writes.join().unwrap();
    cli_writes.join().unwrap();

    let path: PathBuf = dir.path().join("registered.toml");
    let parsed: toml::Value = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(parsed.as_table().unwrap().len(), 200);
}
