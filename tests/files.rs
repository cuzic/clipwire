//! ADR-0012 / Phase 9: 実サーバと実クライアントの結合(Linux / Windows CI)。
mod common;

use std::process::Command;

use common::{TestServer, TEST_TOKEN};

fn client(server: &TestServer) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clipwire"));
    command
        .env("CLIPD_HOST", "127.0.0.1")
        .env("CLIPD_PORT", server.port().to_string())
        .env("CLIPD_TOKEN", TEST_TOKEN);
    command
}

#[test]
fn ac_t9_1_7_send_saves_into_files_dir_and_prints_path() {
    let downloads = tempfile::tempdir().unwrap();
    let dir = downloads.path().to_str().unwrap().to_owned();
    let Some(server) = TestServer::start_with_args(&["--files-dir", &dir]) else {
        return;
    };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("deck.pptx");
    let body: Vec<u8> = (0..200_000_u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&file, &body).unwrap();

    for expected in ["deck.pptx", "deck (1).pptx"] {
        let output = client(&server).arg("send").arg(&file).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let printed = String::from_utf8(output.stdout).unwrap();
        assert!(printed.trim().ends_with(expected), "{printed}");
        assert_eq!(
            std::fs::read(downloads.path().join(expected)).unwrap(),
            body
        );
    }

    // Windows CI: Mark of the Web が付く(ADR-0012 §1)
    #[cfg(windows)]
    {
        let mut stream = downloads.path().join("deck.pptx").into_os_string();
        stream.push(":Zone.Identifier");
        let zone = std::fs::read_to_string(&stream).unwrap();
        assert!(zone.contains("ZoneId=3"), "{zone}");
    }

    let output = client(&server)
        .args(["send", "--name", "../evil.cmd"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(std::fs::read_dir(downloads.path()).unwrap().count(), 2);
}

#[test]
fn ac_t9_2_and_3_info_and_unregister_via_cli() {
    let Some(server) = TestServer::start_with_args(&["--auto-approve"]) else {
        return;
    };
    let output = client(&server).arg("info").output().unwrap();
    assert!(output.status.success());
    let info: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(info["app"], "clipwire");

    ureq::post(&server.url("/register"))
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {TEST_TOKEN}"))
        .send_string(r#"{"name":"tmp-target","target":{"script":"()"}}"#)
        .unwrap();
    let output = client(&server)
        .args(["unregister", "tmp-target"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("tmp-target"));
    let output = client(&server)
        .args(["unregister", "tmp-target"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("404"));
}
