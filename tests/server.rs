mod common;

use std::thread;

use common::TestServer;

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
fn ac_t0_3_2_register_writes_pending_to_the_overridden_config_dir() {
    let Some(server) = TestServer::start() else {
        return;
    };
    let response = ureq::post(&server.url("/register"))
        .set("Content-Type", "application/json")
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
