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
    /// ジョブ一覧を表示
    Jobs,
    /// ローカルと Windows のターゲット状態を表示
    List(ListArgs),
    /// ターゲット状態、実行中ジョブ、サーバー情報を表示
    Status,
    /// ジョブのログを表示
    Logs(JobIdArgs),
    /// 実行中のジョブを終了
    Kill(JobIdArgs),
    /// ターゲットの定義を Windows に送って承認待ちに追加
    Register(RegisterArgs),
    /// 承認待ちターゲットを承認して registered.toml に保存 (Windows ローカルで実行)
    Approve(ApproveArgs),
    /// 承認待ちターゲットの一覧を表示 (Windows ローカルで実行)
    Pending,
    /// 承認待ちターゲットの定義全文を表示 (Windows ローカルで実行)
    Show(LocalTargetArgs),
    /// 承認待ちターゲットを拒否 (Windows ローカルで実行)
    Deny(DenyArgs),
    /// ローカルの監査ログを末尾から表示
    Audit(AuditArgs),
    /// /health を監視し、連続失敗時に動作確認済みの serve で復旧する (Windows)
    Watchdog(watchdog::WatchdogArgs),
}

#[derive(Args, Debug)]
struct RegisterArgs {
    /// 登録するターゲット名 (~/.config/clipwire/targets.toml で定義)
    target: String,
}

#[derive(Args, Debug)]
struct ListArgs {
    /// 安定した JSON 形式で表示
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ApproveArgs {
    /// 承認するターゲット名
    target: String,
    /// 現在の pending 定義の sha256 ハッシュ (prefix 可)
    #[arg(long, value_name = "PREFIX", required = true)]
    hash: String,
}

#[derive(Args, Debug)]
struct LocalTargetArgs {
    /// 承認待ちターゲット名
    target: String,
}

#[derive(Args, Debug)]
struct DenyArgs {
    /// 拒否するターゲット名
    target: String,
    /// 現在の pending 定義の sha256 ハッシュ (prefix 可)
    #[arg(long, value_name = "PREFIX")]
    hash: Option<String>,
}

#[derive(Args, Debug)]
struct AuditArgs {
    /// 表示する末尾の行数
    #[arg(long, default_value = "50")]
    tail: usize,
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
    /// ターゲット定義の上限より短い実行期限
    #[arg(long, value_name = "DURATION")]
    timeout: Option<String>,
    /// ストリーミングせず、完了後に出力をまとめて取得
    #[arg(long)]
    no_stream: bool,
    /// ジョブ ID を即座に返し、バックグラウンドで実行
    #[arg(long)]
    detach: bool,
    /// 実行結果を Windows クリップボードへコピー
    /// 注意: ビルドログにはトークン等が含まれることがあり、コピー内容は他アプリや
    /// クリップボード履歴から参照される可能性があります。
    #[arg(long, conflicts_with = "detach")]
    copy: bool,
    /// 終了コードが 0 でない場合だけ実行結果を Windows クリップボードへコピー
    /// 注意: ビルドログにはトークン等が含まれることがあり、コピー内容は他アプリや
    /// クリップボード履歴から参照される可能性があります。
    #[arg(long, conflicts_with = "detach")]
    copy_on_fail: bool,
    /// クリップボードへのコピー時に ANSI エスケープを維持
    #[arg(long)]
    raw: bool,
}

#[derive(Args, Debug)]
struct JobIdArgs {
    /// ジョブ ID
    id: String,
}

mod audit;
mod client;
mod config;
mod exec_rhai;
mod jobs;
// T4.5 now uses Runner, while some T4.2 API surface remains reserved for the
// timeout/job lifecycle tasks and is intentionally not called yet.
#[allow(dead_code)]
mod runner;
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
        Cmd::Jobs => {
            let cfg = ClientConfig::from_env()?;
            cmd_jobs(&cfg)
        }
        Cmd::List(args) => {
            let cfg = ClientConfig::from_env()?;
            cmd_list(&cfg, &args)
        }
        Cmd::Status => {
            let cfg = ClientConfig::from_env()?;
            cmd_status(&cfg)
        }
        Cmd::Logs(args) => {
            let cfg = ClientConfig::from_env()?;
            cmd_logs(&cfg, &args)
        }
        Cmd::Kill(args) => {
            let cfg = ClientConfig::from_env()?;
            cmd_kill(&cfg, &args)
        }
        Cmd::Register(args) => {
            let cfg = ClientConfig::from_env()?;
            cmd_register(&cfg, &args)
        }
        Cmd::Approve(args) => cmd_approve(&args),
        Cmd::Pending => cmd_pending(),
        Cmd::Show(args) => cmd_show(&args),
        Cmd::Deny(args) => cmd_deny(&args),
        Cmd::Audit(args) => cmd_audit(&args),
        Cmd::Watchdog(args) => watchdog::run_watchdog(args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(windows))]
    use crate::exec_rhai::exec_rhai;
    #[cfg(target_os = "linux")]
    use crate::exec_rhai::exec_rhai_cancelable;
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
            config_dir: config_dir.clone(),
            store,
            auto_approve,
            host_policy: HostPolicy::default(),
            audit: audit::AuditLog::new(config_dir.clone()),
            jobs: jobs::JobRegistry::new(config_dir).unwrap(),
            stream_ping_interval: Duration::from_secs(30),
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
            config_dir: config_dir.clone(),
            store,
            auto_approve: false,
            host_policy: HostPolicy::default(),
            audit: audit::AuditLog::new(config_dir.clone()),
            jobs: jobs::JobRegistry::new(config_dir).unwrap(),
            stream_ping_interval: Duration::from_secs(30),
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
                (
                    RouteId::TargetsCheck,
                    "/targets/check",
                    RouteClass::Protected,
                ),
                (RouteId::Jobs, "/jobs", RouteClass::Protected),
                (RouteId::Job, "/jobs/:id", RouteClass::Protected),
                (RouteId::JobLog, "/jobs/:id/log", RouteClass::Protected),
                (RouteId::JobKill, "/jobs/:id/kill", RouteClass::Protected),
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

    #[tokio::test]
    async fn ac_t7_7_1_open_accepts_json_post_and_legacy_get() {
        let dir = tempdir().unwrap();
        let app = build_router(test_state(dir.path().to_path_buf(), false));
        let post = axum::http::Request::builder()
            .method("POST")
            .uri("/open")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"name":"chatgpt"}"#))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(post).await.unwrap().status(),
            StatusCode::OK
        );

        let get = axum::http::Request::builder()
            .uri("/open?name=claude")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(get).await.unwrap().status(),
            StatusCode::OK
        );

        let missing_content_type = axum::http::Request::builder()
            .method("POST")
            .uri("/open")
            .header(header::HOST, "localhost")
            .body(Body::from(r#"{"name":"chatgpt"}"#))
            .unwrap();
        assert_eq!(
            app.oneshot(missing_content_type).await.unwrap().status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
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

    #[test]
    fn ac_t3_8_2_approve_requires_hash_and_dir_is_removed() {
        assert!(Cli::try_parse_from(["clipwire", "approve", "demo"]).is_err());
        assert!(Cli::try_parse_from(["clipwire", "approve", "demo", "--hash", "0123"]).is_ok());
        let error = Cli::try_parse_from([
            "clipwire", "approve", "demo", "--hash", "0123", "--dir", "C:\\tmp",
        ])
        .err()
        .expect("--dir must be rejected");
        assert!(error.to_string().contains("--dir"));
    }

    #[test]
    fn ac_t3_8_4_http_routes_have_no_approval_or_denial_endpoint() {
        assert!(ROUTES.iter().all(|route| {
            !route.path.contains("approve")
                && !route.path.contains("deny")
                && !route.path.contains("pending")
                && !route.path.contains("show")
        }));
    }

    #[test]
    fn ac_t5_6_1_only_auto_approve_register_may_change_registered() {
        let mutations: Vec<_> = ROUTES
            .iter()
            .map(|route| (route.id, route.path, route.registered_mutation))
            .collect();
        assert_eq!(
            mutations,
            vec![
                (RouteId::Health, "/health", RegisteredMutation::Never),
                (RouteId::Root, "/", RegisteredMutation::Never),
                (RouteId::Clip, "/clip", RegisteredMutation::Never),
                (RouteId::File, "/file", RegisteredMutation::Never),
                (RouteId::VFile, "/vfile", RegisteredMutation::Never),
                (RouteId::Open, "/open", RegisteredMutation::Never),
                (RouteId::Exec, "/exec", RegisteredMutation::Never),
                (
                    RouteId::Register,
                    "/register",
                    RegisteredMutation::AutoApproveOnly,
                ),
                (
                    RouteId::TargetsCheck,
                    "/targets/check",
                    RegisteredMutation::Never,
                ),
                (RouteId::Jobs, "/jobs", RegisteredMutation::Never),
                (RouteId::Job, "/jobs/:id", RegisteredMutation::Never),
                (RouteId::JobLog, "/jobs/:id/log", RegisteredMutation::Never),
                (
                    RouteId::JobKill,
                    "/jobs/:id/kill",
                    RegisteredMutation::Never
                ),
            ]
        );
    }

    #[tokio::test]
    async fn ac_t5_6_2_manual_register_only_creates_pending() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), false);
        let response = handle_register(
            State(state),
            HeaderMap::new(),
            serde_json::to_vec(&serde_json::json!({"name":"manual","script":"()"}))
                .unwrap()
                .into(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!dir.path().join("registered.toml").exists());
        assert!(load_target_map(&dir.path().join("pending.toml"))
            .unwrap()
            .contains_key("manual"));
    }

    #[tokio::test]
    async fn ac_t2_3_1_origin_is_rejected_on_every_route() {
        let dir = tempdir().unwrap();
        let app = build_router(test_state(dir.path().to_path_buf(), false));

        for route in ROUTES {
            let method = match route.id {
                RouteId::Exec | RouteId::Register | RouteId::TargetsCheck | RouteId::JobKill => {
                    "POST"
                }
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
    fn ac_t5_1_2_timeout_is_hashed_only_when_explicit() {
        let unset = script_target("echo stable");
        let mut explicit = unset.clone();
        explicit.timeout = Some("30m".into());
        assert!(!String::from_utf8(canonical_json(&unset))
            .unwrap()
            .contains("timeout"));
        assert_ne!(canonical_json(&unset), canonical_json(&explicit));
        assert_ne!(
            definition_hash(&canonical_json(&unset)),
            definition_hash(&canonical_json(&explicit))
        );
    }

    #[tokio::test]
    async fn ac_t5_1_3_exec_timeout_can_only_shorten_definition() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        let mut target = script_target("let answer = 42;");
        target.timeout = Some("10m".into());
        state
            .store
            .register("limited".into(), target, true)
            .await
            .unwrap();

        let response = handle_exec(
            State(state.clone()),
            HeaderMap::new(),
            serde_json::to_vec(&serde_json::json!({"name":"limited","timeout":"1h"}))
                .unwrap()
                .into(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("10m"));

        let response = handle_exec(
            State(state),
            HeaderMap::new(),
            serde_json::to_vec(&serde_json::json!({"name":"limited","timeout":"5s"}))
                .unwrap()
                .into(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ac_t5_1_4_rhai_loop_sleep_and_run_time_out() {
        // run の子ツリー kill は Unix の Runner でのみ実装済み(Windows は T4.4 まで
        // スタブで、sh もない)ため、run の経路は Unix でだけ検証する。
        let mut scripts = vec!["loop {}", "sleep(10000);"];
        if cfg!(unix) {
            scripts.push(r#"run(["sh", "-c", "sleep 10"]);"#);
        }
        for (index, script) in scripts.into_iter().enumerate() {
            let dir = tempdir().unwrap();
            let state = test_state(dir.path().to_path_buf(), true);
            let mut target = script_target(script);
            target.timeout = Some("100ms".into());
            let name = format!("rhai-timeout-{index}");
            state
                .store
                .register(name.clone(), target, true)
                .await
                .unwrap();
            let started = std::time::Instant::now();
            let response = handle_exec(
                State(state),
                HeaderMap::new(),
                serde_json::to_vec(&serde_json::json!({"name":name}))
                    .unwrap()
                    .into(),
            )
            .await;
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "script={script}"
            );
            assert_eq!(response.headers()["X-Exit-Code"], "124");
            assert_eq!(response.headers()["X-Job-State"], "timeout");
        }
    }

    #[tokio::test]
    async fn ac_t5_1_5_register_rejects_invalid_and_zero_timeouts() {
        let dir = tempdir().unwrap();
        for timeout in ["abc", "-1s", "0s"] {
            let response = handle_register(
                State(test_state(dir.path().to_path_buf(), true)),
                HeaderMap::new(),
                serde_json::to_vec(&serde_json::json!({
                    "name": format!("bad-{timeout}"), "script": "()", "timeout": timeout
                }))
                .unwrap()
                .into(),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "timeout={timeout}"
            );
        }
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
                timeout: None,
                no_stream: false,
                detach: false,
                copy: false,
                copy_on_fail: false,
                raw: false,
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
    #[cfg(not(windows))]
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
        // stdout/stderr are no longer concatenated in fixed stderr-first order;
        // child output and Rhai print now retain their actual production order.
        assert!(output.find("run-ok").unwrap() < output.find("run-fail").unwrap());
        assert!(output.find("run-fail").unwrap() < output.find("run_ok=false").unwrap());
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
    #[cfg(not(windows))]
    fn ac_t4_5_2_child_output_precedes_following_print() {
        let (output, code) = exec_rhai(
            r#"run(["sh", "-c", "echo a; sleep 0.2"]); print("done");"#,
            None,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(output).unwrap(), "a\ndone\n");
    }

    #[test]
    #[cfg(not(windows))]
    fn ac_t4_5_3_and_4_resource_limits_stop_runaway_scripts() {
        for script in ["loop {}", r#"let s = "a"; loop { s += s; }"#] {
            let (output, code) = exec_rhai(script, None).unwrap();
            assert_eq!(code, 1);
            assert!(String::from_utf8(output).unwrap().contains("script error:"));
        }
    }

    #[test]
    #[cfg(not(windows))]
    fn ac_t4_5_5_child_environment_excludes_clipd_token() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLIPD_TOKEN", "ac-t4-5-secret");
        let result = exec_rhai(r#"run(["sh", "-c", "env"]);"#, None);
        std::env::remove_var("CLIPD_TOKEN");
        let (output, code) = result.unwrap();
        assert_eq!(code, 0);
        assert!(!String::from_utf8(output).unwrap().contains("CLIPD_TOKEN="));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn ac_t4_5_6_cancellation_kills_run_process_group_and_returns() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = std::thread::spawn(move || {
            exec_rhai_cancelable(
                r#"run(["sh", "-c", "echo $$; sleep 100 & wait"]);"#,
                None,
                worker_cancelled,
            )
        });
        std::thread::sleep(Duration::from_millis(200));
        cancelled.store(true, Ordering::Relaxed);
        let (output, code) = worker.join().unwrap().unwrap();
        assert_eq!(code, 1);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("script error:"));
        let pid: i32 = output.lines().next().unwrap().parse().unwrap();
        // SAFETY: signal 0 only probes whether the emitted process-group leader remains.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
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
                    timeout: None,
                    no_stream: false,
                    detach: false,
                    copy: false,
                    copy_on_fail: false,
                    raw: false,
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

    async fn response_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn protected_post(path: &str, body: String) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn ac_t3_6_1_and_4_health_preserves_text_and_advertises_protocol() {
        let dir = tempdir().unwrap();
        let app = build_router(test_state(dir.path().to_path_buf(), false));
        let plain = axum::http::Request::builder()
            .uri("/health")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(plain).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
            "OK\n"
        );

        let json = axum::http::Request::builder()
            .uri("/health")
            .header(header::HOST, "localhost")
            .header(header::ACCEPT, "application/json")
            .body(Body::empty())
            .unwrap();
        let value = response_json(app.oneshot(json).await.unwrap()).await;
        assert_eq!(value["proto"], 2);
        assert_eq!(
            value["features"],
            serde_json::json!([
                "hash",
                "timeout",
                "concurrency",
                "jobs",
                "stream",
                "open-post"
            ])
        );
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn ac_t3_6_2_and_3_register_forms_are_equivalent_and_strict() {
        let dir = tempdir().unwrap();
        let mut state = test_state(dir.path().to_path_buf(), true);
        state.allow_no_token = true;
        let app = build_router(state);
        let nested = r#"{"name":"nested","target":{"script":"echo hi","env":{"B":"2","A":"1"}}}"#;
        let flat = r#"{"name":"flat","script":"echo hi","env":{"A":"1","B":"2"}}"#;
        let nested_response = app
            .clone()
            .oneshot(protected_post("/register", nested.into()))
            .await
            .unwrap();
        let flat_response = app
            .clone()
            .oneshot(protected_post("/register", flat.into()))
            .await
            .unwrap();
        let body = |response: Response| async move {
            String::from_utf8(
                axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap()
        };
        let nested_body = body(nested_response).await;
        let flat_body = body(flat_response).await;
        assert_eq!(
            nested_body.lines().find(|line| line.starts_with("hash:")),
            flat_body.lines().find(|line| line.starts_with("hash:"))
        );

        for invalid in [
            r#"{"name":"bad-flat","script":"x","unknown":1}"#,
            r#"{"name":"bad-nested","target":{"script":"x","unknown":1}}"#,
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(protected_post("/register", invalid.into()))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
    }

    #[test]
    fn ac_t3_6_4_and_5_feature_gate_and_old_server_wire_format() {
        let target = script_target("echo compatible");
        let old = client::ServerCapabilities::default();
        let new = client::ServerCapabilities {
            version: "test".into(),
            proto: 2,
            features: vec!["hash".into()],
        };
        assert!(client::require_features(&new, &["hash"]).is_ok());
        assert!(client::require_features(&old, &["future-field"]).is_err());

        #[derive(Deserialize)]
        struct OldRequest {
            name: String,
            #[serde(flatten)]
            target: StoredTarget,
        }
        let nested = client::register_body(&new, "compat", &target);
        let parsed_nested: OldRequest = serde_json::from_value(nested).unwrap();
        assert_eq!(parsed_nested.name, "compat");
        assert!(parsed_nested.target.script.is_none());

        let flat = client::register_body(&old, "compat", &target);
        let parsed_flat: OldRequest = serde_json::from_value(flat).unwrap();
        assert_eq!(
            parsed_flat.target.script.as_deref(),
            Some("echo compatible")
        );
    }

    #[tokio::test]
    async fn ac_t3_7_1_and_2_check_reports_all_states_without_definitions() {
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        store
            .register("ok".into(), script_target("A"), true)
            .await
            .unwrap();
        store
            .register("changed".into(), script_target("A"), true)
            .await
            .unwrap();
        store
            .register("remote".into(), script_target("R"), true)
            .await
            .unwrap();
        store
            .register("pending-same".into(), script_target("P"), false)
            .await
            .unwrap();
        store
            .register("pending-different".into(), script_target("P"), false)
            .await
            .unwrap();
        let mut state = test_state(dir.path().to_path_buf(), false);
        state.allow_no_token = true;
        let app = build_router(state);
        let request = serde_json::json!({"targets": {
            "ok": {"script":"A"},
            "changed": {"script":"B"},
            "pending-same": {"script":"P"},
            "pending-different": {"script":"Q"},
            "new": {"script":"N"}
        }});
        let response = app
            .oneshot(protected_post("/targets/check", request.to_string()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value = response_json(response).await;
        let targets = &value["targets"];
        assert_eq!(targets["ok"]["status"], "ok");
        assert_eq!(targets["changed"]["status"], "changed");
        assert_eq!(targets["pending-same"]["status"], "pending");
        assert_eq!(targets["pending-same"]["pending_matches"], true);
        assert_eq!(targets["pending-different"]["pending_matches"], false);
        assert_eq!(targets["new"]["status"], "unregistered");
        assert_eq!(targets["remote"]["status"], "remote-only");
        let encoded = value.to_string();
        for secret in ["script", "steps", "env", "dir", "echo"] {
            assert!(!encoded.contains(secret));
        }
    }

    #[tokio::test]
    async fn ac_t3_7_3_check_authentication_matches_other_protected_routes() {
        let dir = tempdir().unwrap();
        let mut state = test_state(dir.path().to_path_buf(), false);
        state.token = Some("secret".into());
        let app = build_router(state);
        let body = r#"{"targets":{}}"#;
        assert_eq!(
            app.clone()
                .oneshot(protected_post("/targets/check", body.into()))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let mut wrong = protected_post("/targets/check", body.into());
        wrong
            .headers_mut()
            .insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert_eq!(
            app.clone().oneshot(wrong).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let no_token_app = build_router(test_state(dir.path().to_path_buf(), false));
        assert_eq!(
            no_token_app
                .oneshot(protected_post("/targets/check", body.into()))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn ac_t3_7_4_check_accepts_three_megabytes_and_rejects_over_sixteen_mib() {
        let dir = tempdir().unwrap();
        let mut state = test_state(dir.path().to_path_buf(), false);
        state.allow_no_token = true;
        let app = build_router(state);
        let large_script = "x".repeat(1_600);
        let targets: serde_json::Map<String, serde_json::Value> = (0..2_000)
            .map(|index| {
                (
                    format!("target-{index}"),
                    serde_json::json!({"script": large_script}),
                )
            })
            .collect();
        let body = serde_json::json!({"targets": targets}).to_string();
        assert!(body.len() > 3 * 1024 * 1024);
        assert_eq!(
            app.clone()
                .oneshot(protected_post("/targets/check", body))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let oversized = format!(
            r#"{{"targets":{{"huge":{{"script":"{}"}}}}}}"#,
            "x".repeat(17 * 1024 * 1024)
        );
        assert_eq!(
            app.oneshot(protected_post("/targets/check", oversized))
                .await
                .unwrap()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
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

    fn audit_values(dir: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join(audit::AUDIT_FILE_NAME))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn ac_t5_2_1_register_approve_exec_events_are_ordered_jsonl() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), false);
        let response = handle_register(
            State(state.clone()),
            HeaderMap::new(),
            serde_json::to_vec(&serde_json::json!({"name":"audit-flow","script":"()"}))
                .unwrap()
                .into(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let target = state.store.pending().unwrap()["audit-flow"].clone();
        let hash = definition_hash(&canonical_json(&target));
        state.store.approve("audit-flow", &hash).unwrap();
        let mut approval =
            audit::AuditEvent::new(audit::AuditEventKind::Approve).target("audit-flow", &hash);
        approval.approver = Some("local-user".into());
        state.audit.record(approval);
        let response = handle_exec(
            State(state),
            HeaderMap::new(),
            serde_json::to_vec(&serde_json::json!({"name":"audit-flow"}))
                .unwrap()
                .into(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let values = audit_values(dir.path());
        let events: Vec<_> = values
            .iter()
            .map(|value| value["event"].as_str().unwrap())
            .collect();
        assert_eq!(events, ["register", "approve", "start", "end"]);
    }

    async fn register_exec_target(
        state: &AppState,
        name: &str,
        script: &str,
        concurrency: Option<&str>,
    ) {
        let mut value = serde_json::json!({"name": name, "script": script});
        if let Some(concurrency) = concurrency {
            value["concurrency"] = concurrency.into();
        }
        let response = handle_register(
            State(state.clone()),
            HeaderMap::new(),
            serde_json::to_vec(&value).unwrap().into(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn ac_t6_1_1_and_2_concurrency_is_per_target_and_allow_bypasses_it() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        register_exec_target(&state, "reject", r#"run(["sh", "-c", "sleep 0.4"]);"#, None).await;
        register_exec_target(&state, "other", r#"run(["sh", "-c", "sleep 0.4"]);"#, None).await;
        register_exec_target(
            &state,
            "allow",
            r#"run(["sh", "-c", "sleep 0.4"]);"#,
            Some("allow"),
        )
        .await;

        let execute = |state: AppState, name: &'static str| {
            tokio::spawn(async move {
                handle_exec(
                    State(state),
                    HeaderMap::new(),
                    serde_json::to_vec(&serde_json::json!({"name":name}))
                        .unwrap()
                        .into(),
                )
                .await
            })
        };
        let first = execute(state.clone(), "reject");
        tokio::time::sleep(Duration::from_millis(80)).await;
        let conflict = execute(state.clone(), "reject").await.unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        let conflict_body = String::from_utf8(
            axum::body::to_bytes(conflict.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(conflict_body.contains("ジョブ"), "{conflict_body}");

        let other = execute(state.clone(), "other");
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
        assert_eq!(other.await.unwrap().status(), StatusCode::OK);

        let allow_one = execute(state.clone(), "allow");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let allow_two = execute(state, "allow");
        assert_eq!(allow_one.await.unwrap().status(), StatusCode::OK);
        assert_eq!(allow_two.await.unwrap().status(), StatusCode::OK);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn ac_t6_1_4_rhai_records_child_pid_and_rebuilds_as_orphaned() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        register_exec_target(
            &state,
            "rhai-child",
            r#"run(["sh", "-c", "sleep 1"]);"#,
            None,
        )
        .await;
        let execution = {
            let state = state.clone();
            tokio::spawn(async move {
                handle_exec(
                    State(state),
                    HeaderMap::new(),
                    serde_json::to_vec(&serde_json::json!({"name":"rhai-child"}))
                        .unwrap()
                        .into(),
                )
                .await
            })
        };
        let jobs_dir = dir.path().join("jobs");
        let (id, meta) = loop {
            let found = std::fs::read_dir(&jobs_dir)
                .unwrap()
                .filter_map(Result::ok)
                .find_map(|entry| {
                    let bytes = std::fs::read(entry.path().join("meta.json")).ok()?;
                    let meta: jobs::JobMeta = serde_json::from_slice(&bytes).ok()?;
                    if meta.child.is_some() {
                        Some((meta.id.clone(), meta))
                    } else {
                        None
                    }
                });
            if let Some(found) = found {
                break found;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_ne!(meta.child.unwrap().pid, std::process::id());
        let rebuilt = jobs::JobRegistry::new(dir.path()).unwrap();
        assert_eq!(rebuilt.get(&id).unwrap().state, jobs::JobStatus::Orphaned);
        assert_eq!(execution.await.unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ac_t5_2_2_secrets_environment_and_child_output_are_not_audited() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        let secret_token = "TOKEN_DO_NOT_AUDIT";
        let secret_env = "ENV_DO_NOT_AUDIT";
        let secret_output = "OUTPUT_DO_NOT_AUDIT";
        handle_register(
            State(state.clone()),
            HeaderMap::new(),
            serde_json::to_vec(&serde_json::json!({
                "name":"audit-secret", "script": format!("print(\"{secret_output}\");"),
                "env":{"SECRET":secret_env}
            }))
            .unwrap()
            .into(),
        )
        .await;
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {secret_token}").parse().unwrap(),
        );
        let response = handle_exec(
            State(state),
            headers,
            serde_json::to_vec(&serde_json::json!({"name":"audit-secret"}))
                .unwrap()
                .into(),
        )
        .await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains(secret_output));
        let log = std::fs::read_to_string(dir.path().join(audit::AUDIT_FILE_NAME)).unwrap();
        for secret in [secret_token, secret_env, secret_output] {
            assert!(!log.contains(secret));
        }
    }

    #[test]
    fn ac_t5_2_3_rotates_at_ten_mib_and_keeps_one_generation() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(audit::AUDIT_FILE_NAME);
        std::fs::write(&path, vec![b'x'; audit::AUDIT_ROTATE_BYTES as usize]).unwrap();
        let log = audit::AuditLog::new(dir.path().to_path_buf());
        log.record(audit::AuditEvent::new(audit::AuditEventKind::Start));
        assert_eq!(
            std::fs::metadata(dir.path().join(audit::AUDIT_ROTATED_FILE_NAME))
                .unwrap()
                .len(),
            audit::AUDIT_ROTATE_BYTES
        );
        std::fs::write(&path, vec![b'y'; audit::AUDIT_ROTATE_BYTES as usize]).unwrap();
        log.record(audit::AuditEvent::new(audit::AuditEventKind::End));
        assert_eq!(
            std::fs::read(dir.path().join(audit::AUDIT_ROTATED_FILE_NAME)).unwrap()[0],
            b'y'
        );
        assert!(!dir.path().join("audit.2.jsonl").exists());
    }

    #[derive(Default)]
    struct WarningRecorder(std::sync::atomic::AtomicUsize);

    impl audit::ApprovalAuditWarning for WarningRecorder {
        fn warn(&self, _message: &str) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn ac_t5_2_4_audit_failure_does_not_stop_exec_and_warns_for_approval() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        state
            .store
            .register("audit-io".into(), script_target("()"), true)
            .await
            .unwrap();
        let invalid_root = dir.path().join("not-a-directory");
        std::fs::write(&invalid_root, b"file").unwrap();
        let warning = Arc::new(WarningRecorder::default());
        let mut state = state;
        state.audit = audit::AuditLog::with_warning(invalid_root, warning.clone());
        state
            .audit
            .record(audit::AuditEvent::new(audit::AuditEventKind::Approve));
        assert_eq!(warning.0.load(std::sync::atomic::Ordering::Relaxed), 1);
        let response = handle_exec(
            State(state),
            HeaderMap::new(),
            serde_json::to_vec(&serde_json::json!({"name":"audit-io"}))
                .unwrap()
                .into(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn ac_t5_2_5_audit_is_not_an_http_route() {
        assert!(ROUTES.iter().all(|route| route.path != "/audit"));
    }

    #[tokio::test]
    async fn ac_t5_2_6_def_hash_recovers_the_approved_definition() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        handle_register(
            State(state),
            HeaderMap::new(),
            serde_json::to_vec(&serde_json::json!({"name":"recover","script":"let x = 1;"}))
                .unwrap()
                .into(),
        )
        .await;
        let event = &audit_values(dir.path())[0];
        let digest = event["def_hash"]
            .as_str()
            .unwrap()
            .strip_prefix("sha256:")
            .unwrap();
        let approved =
            std::fs::read(dir.path().join("approved").join(format!("{digest}.json"))).unwrap();
        let expected = canonical_json(&script_target("let x = 1;"));
        assert_eq!(approved, expected);
    }

    #[tokio::test]
    async fn ac_t5_2_7_auto_approve_records_auto_approver_and_approval_record() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        handle_register(
            State(state),
            HeaderMap::new(),
            serde_json::to_vec(&serde_json::json!({"name":"automatic","script":"()"}))
                .unwrap()
                .into(),
        )
        .await;
        let values = audit_values(dir.path());
        assert_eq!(values[1]["event"], "approve");
        assert_eq!(values[1]["approver"], "auto");
        let digest = values[1]["def_hash"]
            .as_str()
            .unwrap()
            .strip_prefix("sha256:")
            .unwrap();
        assert!(dir
            .path()
            .join("approved")
            .join(format!("{digest}.json"))
            .exists());
    }

    #[test]
    fn ac_t5_2_8_auto_approve_serve_start_is_audited() {
        let dir = tempdir().unwrap();
        let log = audit::AuditLog::new(dir.path().to_path_buf());
        log.record(audit::serve_start_event(123, true, true));
        let value = &audit_values(dir.path())[0];
        assert_eq!(value["event"], "serve-start");
        assert_eq!(value["auto_approve"], true);
        assert_eq!(value["auth"], "token");
    }

    #[tokio::test]
    async fn ac_t5_2_9_1571_auto_approved_registers_finish_with_valid_json_under_60s() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        let started = std::time::Instant::now();
        for _ in 0..1571 {
            let response = handle_register(
                State(state.clone()),
                HeaderMap::new(),
                serde_json::to_vec(
                    &serde_json::json!({"name":"bulk-register","script":"let x = 1;"}),
                )
                .unwrap()
                .into(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let elapsed = started.elapsed();
        // 2026-10-01, Linux debug test run in this repository: 1.63 seconds.
        assert!(elapsed < Duration::from_secs(60), "elapsed={elapsed:?}");
        let values = audit_values(dir.path());
        assert_eq!(values.len(), 1571 * 2);
    }

    #[tokio::test]
    async fn ac_t6_2_1_detach_returns_id_and_job_completes() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        handle_register(
            State(state.clone()),
            HeaderMap::new(),
            serde_json::to_vec(
                &serde_json::json!({"name":"detached","script":"sleep(150); print(\"done\");"}),
            )
            .unwrap()
            .into(),
        )
        .await;
        let started = std::time::Instant::now();
        let response = handle_exec(
            State(state.clone()),
            HeaderMap::new(),
            serde_json::to_vec(&serde_json::json!({"name":"detached","detach":true}))
                .unwrap()
                .into(),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let value = response_json(response).await;
        let id = value["id"].as_str().unwrap();
        assert_eq!(state.jobs.get(id).unwrap().state, jobs::JobStatus::Running);
        for _ in 0..100 {
            if state.jobs.get(id).unwrap().state == jobs::JobStatus::Succeeded {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("detached job did not finish");
    }

    #[tokio::test]
    async fn ac_t6_2_2_logs_return_full_output_and_offset() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        let id = state
            .jobs
            .start("log", "hash", None, Concurrency::Allow)
            .unwrap();
        state.jobs.write_log(&id, b"abcdef").unwrap();
        state.jobs.finish(&id, jobs::JobStatus::Succeeded, Some(0));
        let full = server::jobs::log(
            State(state.clone()),
            axum::extract::Path(id.clone()),
            Query(server::jobs::LogQuery {
                offset: 0,
                follow: false,
            }),
        )
        .await;
        let tail = server::jobs::log(
            State(state),
            axum::extract::Path(id),
            Query(server::jobs::LogQuery {
                offset: 3,
                follow: false,
            }),
        )
        .await;
        assert_eq!(
            axum::body::to_bytes(full.into_body(), usize::MAX)
                .await
                .unwrap(),
            "abcdef"
        );
        assert_eq!(
            axum::body::to_bytes(tail.into_body(), usize::MAX)
                .await
                .unwrap(),
            "def"
        );
    }

    async fn next_body_chunk<S>(stream: &mut S) -> axum::body::Bytes
    where
        S: futures_core::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin,
    {
        std::future::poll_fn(|cx| {
            futures_core::Stream::poll_next(std::pin::Pin::new(&mut *stream), cx)
        })
        .await
        .expect("stream ended")
        .expect("infallible body")
    }

    fn ndjson_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "application/x-ndjson".parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn ac_t6_3_1_first_output_arrives_before_completion() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        register_exec_target(
            &state,
            "stream-early",
            r#"print("first"); sleep(300); print("last");"#,
            None,
        )
        .await;
        let response = handle_exec(
            State(state.clone()),
            ndjson_headers(),
            serde_json::to_vec(&serde_json::json!({"name":"stream-early"}))
                .unwrap()
                .into(),
        )
        .await;
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/x-ndjson"
        );
        let mut body = response.into_body().into_data_stream();
        let first = tokio::time::timeout(Duration::from_millis(150), next_body_chunk(&mut body))
            .await
            .expect("first output was buffered until completion");
        let event: serde_json::Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(event["t"], "out");
        assert_eq!(event["d"], "first\n");
        assert_eq!(state.jobs.list(Some(jobs::JobStatus::Running)).len(), 1);
    }

    #[tokio::test]
    async fn ac_t6_3_3_idle_stream_sends_ping() {
        let dir = tempdir().unwrap();
        let mut state = test_state(dir.path().to_path_buf(), true);
        state.stream_ping_interval = Duration::from_millis(10);
        register_exec_target(&state, "stream-ping", "sleep(100);", None).await;
        let response = handle_exec(
            State(state),
            ndjson_headers(),
            serde_json::to_vec(&serde_json::json!({"name":"stream-ping"}))
                .unwrap()
                .into(),
        )
        .await;
        let mut body = response.into_body().into_data_stream();
        let event: serde_json::Value =
            serde_json::from_slice(&next_body_chunk(&mut body).await).unwrap();
        assert_eq!(event, serde_json::json!({"t":"ping"}));
    }

    #[tokio::test]
    async fn ac_t6_3_4_exit_is_last_after_all_output_under_repetition() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        register_exec_target(&state, "stream-order", r#"print("tail");"#, None).await;
        for _ in 0..100 {
            let response = handle_exec(
                State(state.clone()),
                ndjson_headers(),
                serde_json::to_vec(&serde_json::json!({"name":"stream-order"}))
                    .unwrap()
                    .into(),
            )
            .await;
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let events: Vec<serde_json::Value> = bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice(line).unwrap())
                .collect();
            assert_eq!(events.last().unwrap()["t"], "exit");
            assert_eq!(
                events.iter().filter(|event| event["t"] == "exit").count(),
                1
            );
            let output: String = events
                .iter()
                .filter(|event| event["t"] == "out")
                .map(|event| event["d"].as_str().unwrap())
                .collect();
            assert_eq!(output, "tail\n");
        }
    }

    #[tokio::test]
    async fn ac_t6_3_5_and_6_legacy_response_and_missing_status_are_preserved() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        register_exec_target(&state, "legacy-exec", r#"print("done");"#, None).await;
        let response = handle_exec(
            State(state.clone()),
            HeaderMap::new(),
            serde_json::to_vec(&serde_json::json!({"name":"legacy-exec"}))
                .unwrap()
                .into(),
        )
        .await;
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(response.headers()["X-Exit-Code"], "0");
        assert_eq!(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
            "done\n"
        );

        let missing = handle_exec(
            State(state),
            ndjson_headers(),
            serde_json::to_vec(&serde_json::json!({"name":"not-registered"}))
                .unwrap()
                .into(),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn ac_t6_2_3_kill_terminates_tree_changes_state_and_audits() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        register_exec_target(
            &state,
            "killable",
            r#"run(["sh", "-c", "sleep 100 & sleep 100"]);"#,
            None,
        )
        .await;
        let response = handle_exec(
            State(state.clone()),
            HeaderMap::new(),
            serde_json::to_vec(&serde_json::json!({"name":"killable","detach":true}))
                .unwrap()
                .into(),
        )
        .await;
        let id = response_json(response).await["id"]
            .as_str()
            .unwrap()
            .to_string();
        for _ in 0..100 {
            if state.jobs.get(&id).unwrap().child.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let killed = server::jobs::kill(
            State(state.clone()),
            axum::extract::Path(id.clone()),
            "{}".into(),
        )
        .await;
        assert_eq!(killed.status(), StatusCode::ACCEPTED);
        for _ in 0..100 {
            if state.jobs.get(&id).unwrap().state == jobs::JobStatus::Killed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(state.jobs.get(&id).unwrap().state, jobs::JobStatus::Killed);
        assert!(audit_values(dir.path())
            .iter()
            .any(|event| event["event"] == "kill"));
        let again = server::jobs::kill(State(state), axum::extract::Path(id), "{}".into()).await;
        assert_eq!(again.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn ac_t6_2_4_dropped_normal_exec_request_does_not_cancel_job() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), true);
        handle_register(State(state.clone()), HeaderMap::new(), serde_json::to_vec(
            &serde_json::json!({"name":"disconnected","script":"sleep(100); print(\"survived\");"})
        ).unwrap().into()).await;
        let request_state = state.clone();
        let request = tokio::spawn(async move {
            handle_exec(
                State(request_state),
                HeaderMap::new(),
                serde_json::to_vec(&serde_json::json!({"name":"disconnected"}))
                    .unwrap()
                    .into(),
            )
            .await
        });
        let id = loop {
            if let Some(job) = state
                .jobs
                .list(None)
                .into_iter()
                .find(|job| job.target == "disconnected")
            {
                break job.id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        request.abort();
        for _ in 0..100 {
            if state.jobs.get(&id).unwrap().state == jobs::JobStatus::Succeeded {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            state.jobs.get(&id).unwrap().state,
            jobs::JobStatus::Succeeded
        );
        assert_eq!(state.jobs.read_log(&id, 0).unwrap().unwrap(), b"survived\n");
    }

    #[tokio::test]
    async fn ac_t6_2_5_jobs_routes_enforce_auth_origin_and_json() {
        let dir = tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), false);
        let request = |method: &str, path: &str| {
            axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header(header::HOST, "localhost")
                .body(Body::from("{}"))
                .unwrap()
        };
        assert_eq!(
            build_router(state.clone())
                .oneshot(request("GET", "/jobs"))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let mut token_state = state.clone();
        token_state.token = Some("secret".into());
        let wrong = axum::http::Request::builder()
            .uri("/jobs")
            .header(header::HOST, "localhost")
            .header(header::AUTHORIZATION, "Bearer wrong")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            build_router(token_state)
                .oneshot(wrong)
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let mut open_state = state;
        open_state.allow_no_token = true;
        assert_eq!(
            build_router(open_state.clone())
                .oneshot(request("GET", "/jobs"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let origin = axum::http::Request::builder()
            .uri("/jobs")
            .header(header::HOST, "localhost")
            .header(header::ORIGIN, "http://evil")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            build_router(open_state.clone())
                .oneshot(origin)
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let plain = axum::http::Request::builder()
            .method("POST")
            .uri("/jobs/nope/kill")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(
            build_router(open_state)
                .oneshot(plain)
                .await
                .unwrap()
                .status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
    }

    #[tokio::test]
    async fn ac_t6_2_6_unknown_job_ids_return_404() {
        let dir = tempdir().unwrap();
        let mut state = test_state(dir.path().to_path_buf(), false);
        state.allow_no_token = true;
        let app = build_router(state);
        for path in ["/jobs/missing", "/jobs/missing/log"] {
            let request = axum::http::Request::builder()
                .uri(path)
                .header(header::HOST, "localhost")
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::NOT_FOUND
            );
        }
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/jobs/missing/kill")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }
}
