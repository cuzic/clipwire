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

    /// トークンなしで tailnet に公開することを明示許可
    #[arg(long)]
    allow_no_token: bool,

    /// register リクエストを自動承認する (approve 不要)
    #[arg(long)]
    auto_approve: bool,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec_rhai::exec_rhai;
    use proptest::prelude::*;
    use quick_xml::{events::Event, Reader};
    use std::collections::HashMap;
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
        AppState {
            clip_tx,
            token: None,
            last_clip: Arc::new(Mutex::new(LastClip::default())),
            config_dir,
            auto_approve,
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

    fn script_target(script: &str) -> StoredTarget {
        StoredTarget {
            script: Some(script.to_string()),
            ..StoredTarget::default()
        }
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
