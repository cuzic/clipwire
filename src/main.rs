//! clipwire — Tailscale 越しに Windows クリップボードを操作するツール
//!
//! サーバー起動 (Windows):
//!   clipwire serve --token <secret>
//!
//! クリップボード取得 (Linux/Mac/Windows):
//!   clipwire get
//!   clipwire get -q          # quiet: パス/本文だけ出力
//!   clipwire get -w          # Wayland にも書き込む (Linux)
//!   clipwire get -d ~/pics   # 画像保存先を指定
//!
//! クリップボードに書き込み:
//!   echo "hello" | clipwire put
//!   wl-paste | clipwire put

use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use clap::{Args, Parser, Subcommand};
use serde::Deserialize;
use store::Store;
use tokio::sync::oneshot;
use tracing::{info, warn};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "clipwire",
    about = "Tailscale 越しに Windows クリップボードを操作する"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Windows クリップボード HTTP サーバーを起動 (Windows 推奨)
    Serve(ServeArgs),
    /// Windows クリップボードの内容を取得して出力
    Get(GetArgs),
    /// stdin の内容を Windows クリップボードに書き込む
    Put(PutArgs),
    /// Windows のデフォルトブラウザで Web サービスを開く
    Open(OpenArgs),
    /// 登録済みターゲットを Windows で実行して結果を取得
    Exec(ExecArgs),
    /// ターゲットの定義を Windows に送って承認待ちに追加
    Register(RegisterArgs),
    /// 承認待ちターゲットを承認して registered.toml に保存 (Windows ローカルで実行)
    Approve(ApproveArgs),
    /// /health を監視し、連続失敗時に動作確認済みの serve で復旧する (Windows)
    Watchdog(watchdog::WatchdogArgs),
}

#[derive(Args, Debug)]
struct RegisterArgs {
    /// 登録するターゲット名 (~/.config/clipwire/targets.toml で定義)
    target: String,
}

#[derive(Args, Debug)]
struct ApproveArgs {
    /// 承認するターゲット名 (省略時は pending 一覧を表示)
    target: Option<String>,
    /// 実行ディレクトリ (Windows パス)
    #[arg(long, short)]
    dir: Option<String>,
}

#[derive(Args, Debug)]
struct ServeArgs {
    /// 待ち受けポート
    #[arg(long, default_value = "9999")]
    port: u16,

    /// Bearer トークン (コマンドラインへの指定は非推奨)
    #[arg(long)]
    token: Option<String>,

    /// Bearer トークンを読み込むファイル
    #[arg(long, value_name = "PATH")]
    token_file: Option<PathBuf>,

    /// localhost のみにバインド (Tailscale IP をスキップ)
    #[arg(long)]
    bind_localhost_only: bool,

    /// 保護ルートを含め、トークンなしでの利用を明示許可
    #[arg(long)]
    allow_no_token: bool,

    /// register リクエストを自動承認する (approve 不要)
    #[arg(long)]
    auto_approve: bool,

    /// Host ヘッダ不一致時の動作 (初回出荷はログのみ)
    #[arg(long, value_enum, default_value_t = HostCheckMode::Log)]
    host_check: HostCheckMode,

    /// Host ヘッダで追加許可する名前 (複数指定可)
    #[arg(long, value_name = "HOST", action = clap::ArgAction::Append)]
    allow_host: Vec<String>,
}

#[derive(Args, Debug)]
struct GetArgs {
    /// パスや URL だけを出力 (quiet モード)
    #[arg(short, long)]
    quiet: bool,

    /// 画像・大容量テキストの保存先ディレクトリ
    #[arg(short = 'd', long)]
    dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct PutArgs;

#[derive(Args, Debug)]
struct OpenArgs {
    /// 開くサービス
    #[arg(value_enum)]
    target: OpenTarget,
}

#[derive(Debug, Clone, clap::ValueEnum)]
enum OpenTarget {
    /// ChatGPT (https://chatgpt.com)
    Chatgpt,
    /// Claude AI (https://claude.ai)
    Claude,
    /// Tailscale 管理画面 (https://login.tailscale.com/admin)
    Tailscale,
}

impl OpenTarget {
    fn url(&self) -> &'static str {
        match self {
            Self::Chatgpt => "https://chatgpt.com",
            Self::Claude => "https://claude.ai",
            Self::Tailscale => "https://login.tailscale.com/admin",
        }
    }
    fn as_str(&self) -> &'static str {
        match self {
            Self::Chatgpt => "chatgpt",
            Self::Claude => "claude",
            Self::Tailscale => "tailscale",
        }
    }
}

#[derive(Args, Debug)]
struct ExecArgs {
    /// 実行するターゲット名 (~/.config/clipwire/targets.toml で定義)
    target: String,
}

mod client;
mod config;
mod exec_rhai;
mod server;
mod store;
mod watchdog;
#[cfg(windows)]
mod win;

use client::*;
use config::*;
use server::*;
#[cfg(windows)]
use win::win_clip;

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve(args) => {
            install_panic_hook();
            let result = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(run_serve(args));
            if let Err(e) = &result {
                // tracing はここでは既に flush/shutdown 済みの可能性があるため、
                // 直接ファイルへも書く（run_serve が返るのは異常終了のときのみ
                // ——serve_forever は正常時は無限ループするため）。
                let msg = format!("clipd exiting with error: {e:#}\n");
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log_file_path())
                {
                    let _ = f.write_all(msg.as_bytes());
                }
            }
            result
        }
        Cmd::Get(args) => {
            let cfg = ClientConfig::from_env()?;
            cmd_get(&cfg, &args)
        }
        Cmd::Put(_) => {
            let cfg = ClientConfig::from_env()?;
            cmd_put(&cfg)
        }
        Cmd::Open(args) => {
            let cfg = ClientConfig::from_env()?;
            cmd_open(&cfg, &args)
        }
        Cmd::Exec(args) => {
            let cfg = ClientConfig::from_env()?;
            cmd_exec(&cfg, &args)
        }
        Cmd::Register(args) => {
            let cfg = ClientConfig::from_env()?;
            cmd_register(&cfg, &args)
        }
        Cmd::Approve(args) => cmd_approve(&args),
        Cmd::Watchdog(args) => watchdog::run_watchdog(args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec_rhai::exec_rhai;
    use proptest::prelude::*;
    use quick_xml::{events::Event, Reader};
    use std::collections::{BTreeMap, HashMap};
    use tempfile::tempdir;
    use tower::ServiceExt;
    use tracing_subscriber::prelude::*;

    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = LogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            LogWriter(self.0.clone())
        }
    }

    fn capture_logs(action: impl FnOnce()) -> String {
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .without_time()
                .with_writer(capture.clone()),
        );
        tracing::subscriber::with_default(subscriber, action);
        let bytes = capture.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    fn test_state(config_dir: PathBuf, auto_approve: bool) -> AppState {
        let (clip_tx, _clip_rx) = mpsc::sync_channel(1);
        let store = Store::new(config_dir.clone());
        AppState {
            clip_tx,
            token: None,
            allow_no_token: false,
            last_clip: Arc::new(Mutex::new(LastClip::default())),
            config_dir,
            store,
            auto_approve,
            host_policy: HostPolicy::default(),
        }
    }

    fn clipboard_test_state(config_dir: PathBuf) -> AppState {
        let (clip_tx, clip_rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            if let Ok(ClipRequest::GetClip { reply }) = clip_rx.recv() {
                let _ = reply.send(ClipKind::Text("stub".into()));
            }
        });
        let store = Store::new(config_dir.clone());
        AppState {
            clip_tx,
            token: None,
            allow_no_token: false,
            last_clip: Arc::new(Mutex::new(LastClip::default())),
            config_dir,
            store,
            auto_approve: false,
            host_policy: HostPolicy::default(),
        }
    }

    #[test]
    fn ac_t2_1_1_every_route_has_an_explicit_classification() {
        let actual: Vec<_> = ROUTES
            .iter()
            .map(|route| (route.id, route.path, route.class))
            .collect();
        assert_eq!(
            actual,
            vec![
                (RouteId::Health, "/health", RouteClass::Common),
                (RouteId::Root, "/", RouteClass::Clipboard),
                (RouteId::Clip, "/clip", RouteClass::Clipboard),
                (RouteId::File, "/file", RouteClass::Clipboard),
                (RouteId::VFile, "/vfile", RouteClass::Clipboard),
                (RouteId::Open, "/open", RouteClass::Clipboard),
                (RouteId::Exec, "/exec", RouteClass::Protected),
                (RouteId::Register, "/register", RouteClass::Protected),
            ]
        );
    }

    #[tokio::test]
    async fn ac_t2_1_2_middleware_order_and_statuses_are_fixed() {
        let dir = tempdir().unwrap();
        let mut state = test_state(dir.path().to_path_buf(), false);
        state.token = Some("secret".into());
        state.host_policy.mode = HostCheckMode::Enforce;
        let app = build_router(state);

        let request = |host: bool, origin: bool, content_type: Option<&str>| {
            let mut builder = axum::http::Request::builder().method("POST").uri("/exec");
            if host {
                builder = builder.header(header::HOST, "localhost");
            }
            if origin {
                builder = builder.header(header::ORIGIN, "http://evil.example");
            }
            if let Some(content_type) = content_type {
                builder = builder.header(header::CONTENT_TYPE, content_type);
            }
            builder.body(Body::from("{}")).unwrap()
        };

        let cases = [
            (request(false, true, Some("text/plain")), 421),
            (request(true, true, Some("text/plain")), 403),
            (request(true, false, Some("text/plain")), 415),
            (request(true, false, Some("application/json")), 401),
        ];
        for (request, expected) in cases {
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), expected);
        }
    }

    fn host_policy(mode: HostCheckMode, hosts: &[&str]) -> HostPolicy {
        HostPolicy {
            mode,
            allowed: Arc::new(hosts.iter().map(|host| host.to_string()).collect()),
        }
    }

    #[test]
    fn ac_t2_3_2_tailscale_names_include_fqdn_first_label_and_host_name() {
        let names = tailscale_names_from_status(
            br#"{"Self":{"DNSName":"magic-name.tail.example.","HostName":"machine-name"}}"#,
        )
        .unwrap();
        assert_eq!(
            names,
            ["magic-name.tail.example.", "magic-name", "machine-name"]
        );
    }

    #[test]
    fn ac_t2_3_4_cli_defaults_to_log_and_accepts_multiple_allow_hosts() {
        let cli = Cli::try_parse_from([
            "clipwire",
            "serve",
            "--allow-host",
            "foo",
            "--allow-host",
            "BAR",
        ])
        .unwrap();
        let Cmd::Serve(args) = cli.cmd else {
            panic!("expected serve command");
        };
        assert_eq!(args.host_check, HostCheckMode::Log);
        assert_eq!(args.allow_host, ["foo", "BAR"]);
    }

    #[tokio::test]
    async fn ac_t2_3_1_origin_is_rejected_on_every_route() {
        let dir = tempdir().unwrap();
        let app = build_router(test_state(dir.path().to_path_buf(), false));

        for route in ROUTES {
            let method = match route.id {
                RouteId::Exec | RouteId::Register => "POST",
                _ => "GET",
            };
            let request = axum::http::Request::builder()
                .method(method)
                .uri(route.path)
                .header(header::HOST, "localhost")
                .header(header::ORIGIN, "http://evil.example")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::FORBIDDEN,
                "route={}",
                route.path
            );
        }

        let request = axum::http::Request::builder()
            .uri("/health")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ac_t2_3_2_host_allowlist_ignores_port_and_ascii_case() {
        let dir = tempdir().unwrap();
        let mut state = test_state(dir.path().to_path_buf(), false);
        state.host_policy = host_policy(
            HostCheckMode::Enforce,
            &["127.0.0.1", "localhost", "allowed-name"],
        );
        let app = build_router(state);

        for host in ["127.0.0.1:9999", "LOCALHOST", "Allowed-Name:9999"] {
            let request = axum::http::Request::builder()
                .uri("/health")
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap();
            assert_eq!(app.clone().oneshot(request).await.unwrap().status(), 200);
        }
        for host in [Some("evil.example"), None] {
            let mut request = axum::http::Request::builder().uri("/health");
            if let Some(host) = host {
                request = request.header(header::HOST, host);
            }
            assert_eq!(
                app.clone()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                StatusCode::MISDIRECTED_REQUEST
            );
        }
    }

    #[tokio::test]
    async fn ac_t2_3_3_protected_posts_require_json_content_type() {
        let dir = tempdir().unwrap();
        let mut state = test_state(dir.path().to_path_buf(), false);
        state.token = Some("secret".into());
        let app = build_router(state);

        for (content_type, expected) in [
            ("text/plain", StatusCode::UNSUPPORTED_MEDIA_TYPE),
            ("application/json; charset=utf-8", StatusCode::UNAUTHORIZED),
        ] {
            let request = axum::http::Request::builder()
                .method("POST")
                .uri("/exec")
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from("{}"))
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn ac_t2_3_4_allow_host_adds_an_accepted_name() {
        let dir = tempdir().unwrap();
        let mut state = test_state(dir.path().to_path_buf(), false);
        state.host_policy = host_policy(HostCheckMode::Enforce, &["foo"]);
        let request = axum::http::Request::builder()
            .uri("/health")
            .header(header::HOST, "foo")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            build_router(state).oneshot(request).await.unwrap().status(),
            200
        );
    }

    #[tokio::test]
    async fn ac_t2_3_7_log_mode_allows_mismatch_and_enforce_returns_421() {
        let dir = tempdir().unwrap();
        let mut state = test_state(dir.path().to_path_buf(), false);
        state.host_policy = host_policy(HostCheckMode::Log, &["localhost"]);
        let request = || {
            axum::http::Request::builder()
                .uri("/health")
                .header(header::HOST, "evil.example")
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            build_router(state.clone())
                .oneshot(request())
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );

        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "evil.example".parse().unwrap());
        let logs = capture_logs(|| {
            assert!(check_host_header(&state.host_policy, &headers).is_err());
        });
        assert!(logs.contains("WARN"));
        assert!(logs.contains("evil.example"));

        state.host_policy.mode = HostCheckMode::Enforce;
        assert_eq!(
            build_router(state)
                .oneshot(request())
                .await
                .unwrap()
                .status(),
            StatusCode::MISDIRECTED_REQUEST
        );
    }

    #[tokio::test]
    async fn ac_t2_2_1_protected_routes_require_a_configured_token() {
        let dir = tempdir().unwrap();
        let app = build_router(clipboard_test_state(dir.path().to_path_buf()));

        for path in ["/exec", "/register"] {
            let request = axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(String::from_utf8_lossy(&body).contains("--token-file"));
            assert!(String::from_utf8_lossy(&body).contains("--allow-no-token"));
        }

        let request = axum::http::Request::builder()
            .uri("/clip")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ac_t2_2_1b_allow_no_token_accepts_protected_routes_with_browser_guards() {
        let dir = tempdir().unwrap();
        let mut state = test_state(dir.path().to_path_buf(), true);
        state.allow_no_token = true;
        state.host_policy = host_policy(HostCheckMode::Enforce, &["localhost"]);
        let app = build_router(state);

        let request = |path: &'static str, body: &'static str| {
            axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(request(
                    "/register",
                    r#"{"name":"no-token-test","script":"()"}"#,
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            app.clone()
                .oneshot(request("/exec", r#"{"name":"no-token-test"}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );

        let origin = axum::http::Request::builder()
            .method("POST")
            .uri("/exec")
            .header(header::HOST, "localhost")
            .header(header::ORIGIN, "http://evil.example")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(app.clone().oneshot(origin).await.unwrap().status(), 403);

        let non_json = axum::http::Request::builder()
            .method("POST")
            .uri("/exec")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(app.clone().oneshot(non_json).await.unwrap().status(), 415);

        let bad_host = axum::http::Request::builder()
            .method("POST")
            .uri("/exec")
            .header(header::HOST, "evil.example")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(app.oneshot(bad_host).await.unwrap().status(), 421);
    }

    #[tokio::test]
    async fn ac_t2_2_3_correct_token_allows_protected_routes_and_wrong_token_is_401() {
        let dir = tempdir().unwrap();
        let mut state = test_state(dir.path().to_path_buf(), true);
        state.token = Some("secret".into());
        let app = build_router(state);

        let request = |path: &str, token: &str, body: &'static str| {
            axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::from(body))
                .unwrap()
        };

        for path in ["/exec", "/register"] {
            let response = app
                .clone()
                .oneshot(request(path, "wrong", "{}"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }

        let register = request(
            "/register",
            "secret",
            r#"{"name":"token-test","script":"()"}"#,
        );
        assert_eq!(
            app.clone().oneshot(register).await.unwrap().status(),
            StatusCode::OK
        );

        let exec = request("/exec", "secret", r#"{"name":"token-test"}"#);
        assert_eq!(app.oneshot(exec).await.unwrap().status(), StatusCode::OK);
    }

    #[test]
    fn ac_t2_2_4_auto_approve_emits_one_warning_without_the_token() {
        let token = Some("secret-value-must-not-appear".to_string());
        validate_serve_security(true, &token, false).unwrap();
        let logs = capture_logs(|| warn_auto_approve(true));
        assert_eq!(logs.matches("serve-start auto_approve=true").count(), 1);
        assert!(logs.contains("auth=token"));
        assert!(logs.contains("WARN"));
        assert!(!logs.contains(token.as_deref().unwrap()));
    }

    #[test]
    fn ac_t2_2_2_and_2_4_auto_approve_allows_explicit_no_token_and_warns_strongly() {
        let no_token = None;
        let error = validate_serve_security(true, &no_token, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--token-file"));
        assert!(error.contains("--allow-no-token"));

        validate_serve_security(true, &no_token, true).unwrap();
        let logs = capture_logs(|| warn_auto_approve(false));
        assert_eq!(logs.matches("serve-start auto_approve=true").count(), 1);
        assert!(logs.contains("auth=none"));
        assert!(logs.contains("到達できるすべての者"));
        assert!(logs.contains("任意のコード"));
        assert!(logs.contains("承認なし"));
        assert!(logs.contains("WARN"));
    }

    fn script_target(script: &str) -> StoredTarget {
        StoredTarget {
            script: Some(script.to_string()),
            ..StoredTarget::default()
        }
    }

    fn fixture_bytes(path: &str) -> Vec<u8> {
        let mut bytes = std::fs::read(path).unwrap();
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        bytes
    }

    #[test]
    fn ac_t3_1_1_canonical_env_order_is_stable_for_100_constructions() {
        let entries: Vec<_> = (0..11)
            .map(|i| (format!("KEY_{i:02}"), format!("value-{i}")))
            .collect();
        let mut expected = None;
        for seed in 0..100 {
            let mut insertion_order = entries.clone();
            insertion_order.sort_by_key(|(key, _)| {
                key.bytes().fold(seed as u64 + 17, |hash, byte| {
                    hash.wrapping_mul(1099511628211) ^ u64::from(byte)
                })
            });
            let target = StoredTarget {
                script: Some("echo stable".into()),
                env: insertion_order.into_iter().collect(),
                ..StoredTarget::default()
            };
            let bytes = canonical_json(&target);
            if let Some(expected) = &expected {
                assert_eq!(&bytes, expected);
            } else {
                expected = Some(bytes);
            }
        }
    }

    #[test]
    fn ac_t3_1_2_canonical_golden_bytes_and_hashes_are_fixed() {
        let cases = [
            (
                StoredTarget {
                    dir: Some(r"C:\work".into()),
                    script: Some("Write-Output 'ok'".into()),
                    env: BTreeMap::from([
                        ("ZULU".into(), "two".into()),
                        ("ALPHA".into(), "one".into()),
                    ]),
                    ..StoredTarget::default()
                },
                "tests/fixtures/canonical/script.json",
                "sha256:9bf3420dc591ee601f922597f1b029520676d3a6b7baf7f1158c7d9d7dc016d9",
            ),
            (
                StoredTarget {
                    steps: Some(StepsDef::Text("echo one\necho two".into())),
                    ..StoredTarget::default()
                },
                "tests/fixtures/canonical/steps-text.json",
                "sha256:add887b1248be49d82a4c6b6d4bc6bc84495c69e6561336700d1275269e886bd",
            ),
            (
                StoredTarget {
                    steps: Some(StepsDef::Argv(vec![
                        vec!["echo".into(), "one".into()],
                        vec!["echo".into(), "two".into()],
                    ])),
                    env: BTreeMap::from([("LANG".into(), "ja_JP.UTF-8".into())]),
                    ..StoredTarget::default()
                },
                "tests/fixtures/canonical/steps-argv.json",
                "sha256:dbabf65f7f74f7d91e065503472096da52c1ad3311a89b9d78a70f4e94bd8b12",
            ),
        ];
        for (target, fixture, hash) in cases {
            let bytes = canonical_json(&target);
            assert_eq!(bytes, fixture_bytes(fixture), "fixture={fixture}");
            assert_eq!(definition_hash(&bytes), hash, "fixture={fixture}");
        }
    }

    #[test]
    fn ac_t3_1_3_text_and_argv_steps_remain_distinct() {
        let text = StoredTarget {
            steps: Some(StepsDef::Text("echo one".into())),
            ..StoredTarget::default()
        };
        let argv = StoredTarget {
            steps: Some(StepsDef::Argv(vec![vec!["echo".into(), "one".into()]])),
            ..StoredTarget::default()
        };
        assert_ne!(canonical_json(&text), canonical_json(&argv));
        assert_ne!(
            definition_hash(&canonical_json(&text)),
            definition_hash(&canonical_json(&argv))
        );
    }

    #[test]
    fn ac_t3_1_4_unset_future_field_does_not_change_bytes() {
        #[derive(serde::Serialize)]
        struct FutureCanonical<'a> {
            v: u8,
            script: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            timeout: Option<u64>,
        }
        let target = StoredTarget {
            script: Some("echo stable".into()),
            ..StoredTarget::default()
        };
        assert_eq!(
            canonical_json(&target),
            serde_json::to_vec(&FutureCanonical {
                v: 1,
                script: "echo stable",
                timeout: None,
            })
            .unwrap()
        );
    }

    #[test]
    fn ac_t3_2_1_and_2_definition_character_contract() {
        for forbidden in [
            '\u{202e}', '\u{2066}', '\u{200b}', '\u{200d}', '\u{feff}', '\u{00ad}', '\u{2028}',
            '\u{2029}', '\u{3164}', '\0', '\r',
        ] {
            let target = script_target(&format!("safe{forbidden}text"));
            assert!(
                validate_definition(&target).is_err(),
                "accepted U+{:04X}",
                forbidden as u32
            );
        }
        let mut target = script_target("日本語　ok\tline\nCRLF\r\n😀");
        target.dir = Some("C:\\通常".into());
        target.env = BTreeMap::from([("NORMAL_KEY".into(), "通常の値 😀".into())]);
        assert_eq!(validate_definition(&target), Ok(()));
    }

    #[tokio::test]
    async fn ac_t3_2_3_register_returns_position_and_codepoint_in_400() {
        let dir = tempdir().unwrap();
        let body = serde_json::to_vec(&serde_json::json!({
            "name": "bad-definition",
            "script": "first\nabc\u{202e}def"
        }))
        .unwrap();
        let response = handle_register(
            State(test_state(dir.path().to_path_buf(), false)),
            HeaderMap::new(),
            body.into(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8(body.to_vec())
            .unwrap()
            .contains("行 2 桁 4: U+202E"));
        assert!(!dir.path().join("pending.toml").exists());
        assert!(!dir.path().join("registered.toml").exists());
    }

    #[tokio::test]
    async fn ac_t3_5_4_register_response_contains_server_definition_hash() {
        let dir = tempdir().unwrap();
        let target = script_target("let answer = 42;");
        let expected = definition_hash(&canonical_json(&target));
        let body = serde_json::to_vec(&serde_json::json!({
            "name": "hash-response",
            "script": "let answer = 42;"
        }))
        .unwrap();
        let response = handle_register(
            State(test_state(dir.path().to_path_buf(), true)),
            HeaderMap::new(),
            body.into(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8(body.to_vec())
            .unwrap()
            .contains(&format!("hash: {expected}")));
    }

    #[tokio::test]
    async fn ac_t3_4_1_and_3_exec_uses_migrated_record_and_rejects_tampering() {
        let dir = tempdir().unwrap();
        let registered = TargetMap::from([("safe".into(), script_target("let answer = 42;"))]);
        save_target_map(&dir.path().join("registered.toml"), &registered).unwrap();
        let state = test_state(dir.path().to_path_buf(), false);
        state.store.migrate().await.unwrap();
        let request = || {
            serde_json::to_vec(&serde_json::json!({"name": "safe"}))
                .unwrap()
                .into()
        };
        let response = handle_exec(State(state.clone()), HeaderMap::new(), request()).await;
        assert_eq!(response.status(), StatusCode::OK);

        let migrated = load_target_map(&dir.path().join("registered.toml")).unwrap();
        let digest = migrated["safe"]
            .hash
            .as_deref()
            .unwrap()
            .strip_prefix("sha256:")
            .unwrap();
        std::fs::write(
            dir.path().join("approved").join(format!("{digest}.json")),
            b"tampered",
        )
        .unwrap();
        let response = handle_exec(State(state), HeaderMap::new(), request()).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("承認検証エラー"));
    }

    fn validate_targets_file(path: &Path) {
        let source = std::fs::read_to_string(path).unwrap();
        let file: TargetsFile = toml::from_str(&source).unwrap();
        for (name, target) in file.targets {
            validate_target_name(&name).unwrap();
            validate_definition(&target).unwrap();
        }
    }

    #[test]
    fn ac_t3_2_4_real_or_synthetic_targets_fixture_passes() {
        if let Some(path) = std::env::var_os("CLIPWIRE_TARGETS_FIXTURE") {
            validate_targets_file(Path::new(&path));
            return;
        }
        let dir = tempdir().unwrap();
        let path = dir.path().join("targets.toml");
        let long_ascii = format!("powershell -EncodedCommand {}", "QQ==".repeat(10_000));
        let file = toml::to_string(&serde_json::json!({
            "targets": {
                "long-script": { "script": long_ascii },
                "normal-steps": {
                    "steps": [["pwsh", "-Command", "Write-Output '日本語 😀'"]],
                    "env": { "NORMAL": "value" }
                }
            }
        }))
        .unwrap();
        std::fs::write(&path, file).unwrap();
        validate_targets_file(&path);
    }

    fn invalid_target_names() -> Vec<String> {
        vec![
            "".into(),
            "a".repeat(65),
            ".a".into(),
            "-a".into(),
            "a..b".into(),
            "a/b".into(),
            "a b".into(),
            "a'b".into(),
            "a<b".into(),
            "a\u{2018}b".into(),
            "a\0b".into(),
            "日本語".into(),
            "a\nb".into(),
        ]
    }

    #[test]
    fn target_name_validation_matches_contract() {
        for name in invalid_target_names() {
            assert!(validate_target_name(&name).is_err(), "accepted {name:?}");
        }
        for name in ["awase-build", "a", "a.b_c-d", &"a".repeat(64)] {
            assert!(validate_target_name(name).is_ok(), "rejected {name:?}");
        }
    }

    #[test]
    fn singleton_mutex_name_is_stable_and_scoped_to_config_dir() {
        let first = singleton_mutex_name(Path::new("/tmp/clipwire-a"));
        assert_eq!(first, singleton_mutex_name(Path::new("/tmp/clipwire-a")));
        assert_ne!(first, singleton_mutex_name(Path::new("/tmp/clipwire-b")));
        assert!(first.starts_with("Global\\clipwire_singleton_"));
    }

    #[test]
    fn pid_file_overwrites_stale_content_and_is_removed_on_drop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(PID_FILE_NAME);
        std::fs::write(&path, "stale\n").unwrap();

        let guard = PidFile::create_with_pid(dir.path(), 4242).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "4242\n");

        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn pid_file_drop_does_not_remove_a_replacement_servers_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(PID_FILE_NAME);
        let guard = PidFile::create_with_pid(dir.path(), 4242).unwrap();
        std::fs::write(&path, "4343\n").unwrap();

        drop(guard);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "4343\n");
    }

    #[tokio::test]
    async fn invalid_register_names_return_400_without_changing_stores() {
        let dir = tempdir().unwrap();
        let pending_path = dir.path().join("pending.toml");
        let registered_path = dir.path().join("registered.toml");
        let original = HashMap::from([("existing".to_string(), script_target("()"))]);
        save_target_map(&pending_path, &original).unwrap();
        save_target_map(&registered_path, &original).unwrap();
        let pending_before = std::fs::read(&pending_path).unwrap();
        let registered_before = std::fs::read(&registered_path).unwrap();

        for name in invalid_target_names() {
            let body =
                serde_json::to_vec(&serde_json::json!({"name": name, "script": "()"})).unwrap();
            let response = handle_register(
                State(test_state(dir.path().to_path_buf(), false)),
                HeaderMap::new(),
                body.into(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "name={name:?}");
            assert_eq!(std::fs::read(&pending_path).unwrap(), pending_before);
            assert_eq!(std::fs::read(&registered_path).unwrap(), registered_before);
        }
    }

    #[tokio::test]
    async fn invalid_exec_names_return_400_before_store_access() {
        let dir = tempdir().unwrap();
        for name in invalid_target_names() {
            let response = handle_exec(
                State(test_state(dir.path().join("missing"), false)),
                HeaderMap::new(),
                serde_json::to_vec(&serde_json::json!({"name": name}))
                    .unwrap()
                    .into(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn clients_reject_invalid_names_before_network_access() {
        let cfg = ClientConfig {
            host: "invalid.invalid".into(),
            port: 1,
            token: None,
        };
        let exec_error = cmd_exec(
            &cfg,
            &ExecArgs {
                target: "../bad".into(),
            },
        )
        .unwrap_err()
        .to_string();
        let register_error = cmd_register(
            &cfg,
            &RegisterArgs {
                target: "../bad".into(),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(exec_error.contains("無効なターゲット名"));
        assert!(register_error.contains("無効なターゲット名"));
    }

    #[test]
    fn existing_invalid_names_allow_startup_and_emit_one_warning() {
        let dir = tempdir().unwrap();
        let map = HashMap::from([
            ("valid".to_string(), script_target("()")),
            ("../bad".to_string(), script_target("()")),
        ]);
        save_target_map(&dir.path().join("registered.toml"), &map).unwrap();
        let logs = capture_logs(|| warn_invalid_stored_target_names(dir.path()));
        assert_eq!(logs.matches("無効なターゲット名が存在します").count(), 1);
        assert!(logs.contains("../bad"));
        assert_eq!(
            load_target_map(&dir.path().join("registered.toml"))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn auth_results_match_the_existing_contract() {
        let token = Some("secret".to_string());
        for (header_value, expected) in [
            (Some("Bearer secret"), true),
            (Some("Bearer secreu"), false),
            (Some("Bearer short"), false),
            (Some("secret"), false),
            (Some("bearer secret"), false),
            (None, false),
        ] {
            let mut headers = HeaderMap::new();
            if let Some(value) = header_value {
                headers.insert(header::AUTHORIZATION, value.parse().unwrap());
            }
            assert_eq!(check_auth(&token, &headers), expected, "{header_value:?}");
        }
        assert!(check_auth(&None, &HeaderMap::new()));
    }

    #[test]
    fn ac_t0_2_2_steps_and_target_map_behavior_is_preserved() {
        let steps = StepsDef::Text("# comment\n echo one\n\ninvalid 'quote\necho two\n".into());
        assert_eq!(
            steps.into_argv(),
            vec![
                vec!["echo".to_string(), "one".to_string()],
                vec!["echo".to_string(), "two".to_string()]
            ]
        );

        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/targets.toml");
        assert!(load_target_map(&path).unwrap().is_empty());
        let mut targets = TargetMap::default();
        targets.insert("sample".into(), script_target("echo sample"));
        save_target_map(&path, &targets).unwrap();
        let loaded = load_target_map(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["sample"].script.as_deref(), Some("echo sample"));
    }

    #[test]
    fn ac_t0_2_2_exec_rhai_functions_preserve_success_and_failure_behavior() {
        let dir = tempdir().unwrap();
        let existing = dir.path().join("existing.txt");
        std::fs::write(&existing, "present").unwrap();
        let removable = dir.path().join("remove.txt");
        std::fs::write(&removable, "remove").unwrap();

        let script = r#"
            run(["sh", "-c", "printf run-ok"]);
            if !run_ok(["sh", "-c", "printf run-fail >&2; exit 7"]) { print("run_ok=false"); }
            if !run_ok(["clipwire-command-that-does-not-exist"]) { print("missing=false"); }
            if file_exists("existing.txt") && !file_exists("absent.txt") { print("exists-ok"); }
            if rm("remove.txt") && !rm("absent.txt") { print("rm-ok"); }
        "#;
        let (output, code) = exec_rhai(script, dir.path().to_str()).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(code, 0);
        assert!(output.contains("run-ok"));
        assert!(output.contains("run-fail"));
        assert!(output.contains("run_ok: clipwire-command-that-does-not-exist"));
        assert!(!removable.exists());

        let (output, code) = exec_rhai("run([\"sh\", \"-c\", \"exit 9\"]);", None).unwrap();
        assert_eq!(code, 1);
        assert!(String::from_utf8(output).unwrap().contains("exit code 9"));

        let (output, code) =
            exec_rhai("run([\"clipwire-command-that-does-not-exist\"]);", None).unwrap();
        assert_eq!(code, 1);
        assert!(String::from_utf8(output).unwrap().contains("script error"));
    }

    #[test]
    fn token_resolution_honors_priority_and_file_errors() {
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("token");
        std::fs::write(&token_path, "  file-token\r\n").unwrap();
        assert_eq!(
            resolve_serve_token(Some("cli-token"), Some(&token_path), Some("env-token")).unwrap(),
            (Some("cli-token".into()), true)
        );
        assert_eq!(
            resolve_serve_token(None, Some(&token_path), Some("env-token")).unwrap(),
            (Some("file-token".into()), false)
        );
        assert_eq!(
            resolve_serve_token(None, None, Some("env-token")).unwrap(),
            (Some("env-token".into()), false)
        );
        std::fs::write(&token_path, " \n").unwrap();
        assert!(resolve_serve_token(None, Some(&token_path), None).is_err());
        assert!(resolve_serve_token(None, Some(&dir.path().join("missing")), None).is_err());
    }

    #[test]
    fn token_file_accepts_utf8_bom() {
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("token");
        std::fs::write(&token_path, "\u{feff}secret-token\r\n").unwrap();
        assert_eq!(
            resolve_serve_token(None, Some(&token_path), None).unwrap(),
            (Some("secret-token".into()), false)
        );
    }

    #[test]
    fn every_token_source_produces_a_working_auth_token() {
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("token");
        std::fs::write(&token_path, "secret\n").unwrap();
        let sources = [
            resolve_serve_token(Some("secret"), None, None).unwrap().0,
            resolve_serve_token(None, Some(&token_path), None)
                .unwrap()
                .0,
            resolve_serve_token(None, None, Some("secret")).unwrap().0,
        ];
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        for token in sources {
            assert!(check_auth(&token, &headers));
        }
    }

    #[test]
    fn cli_token_warning_never_contains_the_token() {
        let logs = capture_logs(|| warn!("{}", CLI_TOKEN_DEPRECATION_WARNING));
        assert!(logs.contains("--token"));
        assert!(!logs.contains("secret-value"));
    }

    #[test]
    fn cmd_get_error_does_not_expose_token() {
        let token = "get-token-must-not-leak";
        let cfg = ClientConfig {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port: 1,
            token: Some(token.into()),
        };
        let error = cmd_get(
            &cfg,
            &GetArgs {
                quiet: false,
                dir: None,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(!error.contains(token));
    }

    #[test]
    fn put_exec_and_register_errors_do_not_expose_token() {
        let token = "mutation-token-must-not-leak";
        let cfg = ClientConfig {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port: 1,
            token: Some(token.into()),
        };
        let errors = [
            cmd_put(&cfg).unwrap_err().to_string(),
            cmd_exec(
                &cfg,
                &ExecArgs {
                    target: "valid".into(),
                },
            )
            .unwrap_err()
            .to_string(),
            cmd_register(
                &cfg,
                &RegisterArgs {
                    target: "../invalid".into(),
                },
            )
            .unwrap_err()
            .to_string(),
        ];
        for error in errors {
            assert!(!error.contains(token), "token leaked in error: {error}");
        }
    }

    #[test]
    fn generated_download_commands_reference_but_do_not_expand_token() {
        let token = "test-token-must-not-leak";
        let command = download_command("http://127.0.0.1/file", "out", true).unwrap();
        assert!(command.contains("$CLIPD_TOKEN"));
        assert!(!command.contains(token));
        let no_auth = download_command("http://127.0.0.1/file", "out", false).unwrap();
        assert!(!no_auth.contains("Authorization"));
    }

    #[test]
    #[cfg(unix)] // 生成物は POSIX シェル用。/bin/sh で検証する
    fn shell_expands_download_command_token_header() {
        let dir = tempdir().unwrap();
        let mock_curl = dir.path().join("curl");
        std::fs::write(
            &mock_curl,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CAPTURE\"\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = -o ]; then printf ok > \"$2\"; exit 0; fi\n  shift\ndone\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&mock_curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let capture = dir.path().join("args");
        let output = dir.path().join("download");
        let command = download_command("http://mock.invalid/file", "download", true).unwrap();
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &command])
            .current_dir(dir.path())
            .env("CLIPD_TOKEN", "shell-token")
            .env("CAPTURE", &capture)
            .env("PATH", dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(std::fs::read_to_string(capture)
            .unwrap()
            .contains("Authorization: Bearer shell-token\n"));
        assert_eq!(std::fs::read(output).unwrap(), b"ok");
    }

    #[test]
    #[cfg(unix)] // 生成物は POSIX シェル用。/bin/sh で検証する
    fn download_command_quotes_untrusted_names_and_stays_in_directory() {
        let dir = tempdir().unwrap();
        let mock_curl = dir.path().join("curl");
        std::fs::write(
            &mock_curl,
            "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = -o ]; then printf ok > \"$2\"; exit 0; fi\n  shift\ndone\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&mock_curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let outside = dir.path().parent().unwrap().join("clipwire-outside");
        let malicious = format!("../x'$(touch {})", outside.display());
        let command = download_command(
            "http://mock.invalid/file?x='$(touch URL_PWNED)'",
            &malicious,
            false,
        )
        .unwrap();
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &command])
            .current_dir(dir.path())
            .env("PATH", dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!outside.exists());
        assert!(!dir.path().join("URL_PWNED").exists());
        assert_eq!(
            std::fs::read(dir.path().join(safe_download_name(&malicious))).unwrap(),
            b"ok"
        );
    }

    #[test]
    fn source_does_not_restore_powershell_balloon_fallback() {
        let source = include_str!("main.rs");
        for forbidden in [
            ["ShowBalloon", "Tip"].concat(),
            ["Notify", "Icon"].concat(),
            ["Command::new(\"power", "shell\""].concat(),
        ] {
            assert!(
                !source.contains(&forbidden),
                "forbidden source pattern: {forbidden}"
            );
        }
    }

    #[test]
    fn xml_escape_replaces_entities_and_removes_forbidden_controls() {
        assert_eq!(xml_escape("&<>\"'"), "&amp;&lt;&gt;&quot;&apos;");
        assert_eq!(xml_escape("a\0\u{1f}\tb\nc\rd"), "a\tb\nc\rd");
    }

    proptest! {
        #[test]
        fn escaped_xml_round_trips(value in any::<String>().prop_filter(
            "XML 1.0 characters only",
            |s| s.chars().all(|ch| matches!(ch, '\u{9}' | '\u{a}' | '\u{d}')
                || ('\u{20}'..='\u{d7ff}').contains(&ch)
                || ('\u{e000}'..='\u{fffd}').contains(&ch)
                || ('\u{10000}'..='\u{10ffff}').contains(&ch))
        )) {
            let document = format!("<x>{}</x>", xml_escape(&value));
            let mut reader = Reader::from_str(&document);
            let encoded = loop {
                match reader.read_event().unwrap() {
                    Event::Start(start) => break reader.read_text(start.name()).unwrap(),
                    Event::Eof => panic!("missing element"),
                    _ => {}
                }
            };
            let decoded = quick_xml::escape::unescape(&encoded.decode().unwrap()).unwrap().into_owned();
            prop_assert_eq!(decoded, value);
        }
    }
}
