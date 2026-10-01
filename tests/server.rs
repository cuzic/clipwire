mod common;

use std::{process::Command, thread};

use common::TestServer;

#[test]
fn ac_t2_2_2_auto_approve_without_token_fails_before_listening() {
    let config_dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_clipwire"))
        .args(["serve", "--bind-localhost-only", "--auto-approve"])
        .env_remove("CLIPD_TOKEN")
        .env("CLIPWIRE_CONFIG_DIR", config_dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--auto-approve"));
    assert!(stderr.contains("--token-file"));
    assert!(stderr.contains("--allow-no-token"));
}

#[test]
fn ac_t2_2_2_auto_approve_with_allow_no_token_starts() {
    let Some(server) = TestServer::start_auto_approve_no_token() else {
        return;
    };
    assert_eq!(
        ureq::get(&server.url("/health")).call().unwrap().status(),
        200
    );
}

#[test]
fn ac_t0_3_1_health_returns_ok() {
    let Some(server) = TestServer::start() else {
        return;
    };
    let response = ureq::get(&server.url("/health")).call().unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.into_string().unwrap(), "OK\n");
}

#[test]
fn ac_t2_4_b_serve_writes_its_pid_file_to_the_config_directory() {
    let Some(server) = TestServer::start() else {
        return;
    };
    let pid = std::fs::read_to_string(server.config_dir().join("clipwire.pid")).unwrap();
    assert_eq!(pid.trim(), server.process_id().to_string());
}

#[test]
fn ac_t0_3_2_register_writes_pending_to_the_overridden_config_dir() {
    let Some(server) = TestServer::start() else {
        return;
    };
    let response = ureq::post(&server.url("/register"))
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {}", common::TEST_TOKEN))
        .send_string(
            &serde_json::json!({
                "name": "integration-target",
                "script": "echo integration"
            })
            .to_string(),
        )
        .unwrap();
    assert_eq!(response.status(), 200);

    let pending = std::fs::read_to_string(server.config_dir().join("pending.toml")).unwrap();
    assert!(pending.contains("integration-target"));
    assert!(pending.contains("echo integration"));
}

#[test]
fn ac_t3_3_2_corrupt_pending_returns_500_without_replacing_it_with_an_empty_map() {
    let Some(server) = TestServer::start() else {
        return;
    };
    let pending = server.config_dir().join("pending.toml");
    std::fs::write(&pending, "broken = [toml").unwrap();
    let error = ureq::post(&server.url("/register"))
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {}", common::TEST_TOKEN))
        .send_string(
            &serde_json::json!({"name": "must-not-appear", "script": "echo no"}).to_string(),
        )
        .unwrap_err();
    assert!(matches!(error, ureq::Error::Status(500, _)));
    assert!(!pending.exists());
    let corrupt = std::fs::read_dir(server.config_dir())
        .unwrap()
        .flatten()
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("pending.toml.corrupt.")
        })
        .expect("corrupt file must be quarantined");
    let contents = std::fs::read_to_string(corrupt.path()).unwrap();
    assert_eq!(contents, "broken = [toml");
    assert!(!contents.contains("must-not-appear"));
}

#[test]
fn ac_t0_3_3_parallel_servers_use_distinct_ports_and_directories() {
    let first = thread::spawn(TestServer::start);
    let second = thread::spawn(TestServer::start);
    let Some(first) = first.join().unwrap() else {
        return;
    };
    let Some(second) = second.join().unwrap() else {
        return;
    };

    assert_ne!(first.port(), second.port());
    assert_ne!(first.config_dir(), second.config_dir());
    assert_eq!(
        ureq::get(&first.url("/health")).call().unwrap().status(),
        200
    );
    assert_eq!(
        ureq::get(&second.url("/health")).call().unwrap().status(),
        200
    );
}
