use super::*;

mod clip;
mod exec;
mod http_surface;
mod register;

use crate::audit::AuditLog;
use clip::*;
#[cfg(test)]
pub(crate) use exec::handle_exec;
pub(crate) use http_surface::build_router;
#[cfg(test)]
pub(crate) use http_surface::{check_host_header, RegisteredMutation, RouteClass, RouteId, ROUTES};
#[cfg(test)]
pub(crate) use register::handle_register;
pub(crate) use register::handle_targets_check;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum HostCheckMode {
    #[default]
    Log,
    Enforce,
}

#[derive(Clone)]
pub(crate) struct HostPolicy {
    pub(crate) mode: HostCheckMode,
    pub(crate) allowed: Arc<std::collections::BTreeSet<String>>,
}

impl Default for HostPolicy {
    fn default() -> Self {
        Self {
            mode: HostCheckMode::Log,
            allowed: Arc::new(
                ["127.0.0.1".to_string(), "localhost".to_string()]
                    .into_iter()
                    .collect(),
            ),
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum ClipKind {
    Image(Vec<u8>),
    Files(Vec<String>),
    VFiles(Vec<String>),
    Audio(Vec<u8>),
    Html(String),
    Url(String),
    Rtf(String),
    Text(String),
    Empty,
}

#[allow(dead_code)]
pub(crate) enum ClipRequest {
    GetClip {
        reply: oneshot::Sender<ClipKind>,
    },
    SetClip {
        text: String,
        reply: oneshot::Sender<Result<()>>,
    },
    GetFile {
        path: String,
        reply: oneshot::Sender<Option<Vec<u8>>>,
    },
    GetVFile {
        index: usize,
        reply: oneshot::Sender<Option<Vec<u8>>>,
    },
}

#[derive(Clone, Default)]
pub(crate) struct LastClip {
    files: Vec<String>,
    vfiles: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) clip_tx: mpsc::SyncSender<ClipRequest>,
    pub(crate) token: Option<String>,
    pub(crate) allow_no_token: bool,
    pub(crate) last_clip: Arc<Mutex<LastClip>>,
    pub(crate) config_dir: PathBuf,
    pub(crate) store: Store,
    pub(crate) auto_approve: bool,
    pub(crate) host_policy: HostPolicy,
    pub(crate) audit: AuditLog,
}

// ── Logging / panic visibility (serve) ────────────────────────────────────────

/// ログファイルのパス（`clipwire_config_dir()/clipd.log`）。
pub(crate) fn log_file_path() -> PathBuf {
    clipwire_config_dir().join("clipd.log")
}

pub(crate) const PID_FILE_NAME: &str = "clipwire.pid";

pub(crate) struct PidFile {
    path: PathBuf,
    contents: String,
}

impl PidFile {
    fn create(config_dir: &Path) -> Result<Self> {
        Self::create_with_pid_impl(config_dir, std::process::id())
    }

    #[cfg(test)]
    pub(crate) fn create_with_pid(config_dir: &Path, pid: u32) -> Result<Self> {
        Self::create_with_pid_impl(config_dir, pid)
    }

    fn create_with_pid_impl(config_dir: &Path, pid: u32) -> Result<Self> {
        std::fs::create_dir_all(config_dir).with_context(|| {
            format!(
                "PID ファイル用の設定ディレクトリを作成できません: {}",
                config_dir.display()
            )
        })?;
        let path = config_dir.join(PID_FILE_NAME);
        let contents = format!("{pid}\n");
        std::fs::write(&path, &contents)
            .with_context(|| format!("PID ファイルを書き込めません: {}", path.display()))?;
        Ok(Self { path, contents })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        // A replacement server may have already overwritten the stale file.
        // Only remove the file while it still identifies this process.
        if matches!(std::fs::read_to_string(&self.path), Ok(ref contents) if contents == &self.contents)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let thread = std::thread::current();
        let msg = format!(
            "[{ts}] PANIC on thread '{}': {info}\n",
            thread.name().unwrap_or("<unnamed>")
        );
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file_path())
        {
            let _ = f.write_all(msg.as_bytes());
        }
        default_hook(info);
    }));
}

// ── Network helpers (serve) ───────────────────────────────────────────────────

pub(crate) fn find_tailscale_ip() -> Option<Ipv4Addr> {
    if let Ok(out) = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
    {
        if out.status.success() {
            if let Ok(ip) = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<Ipv4Addr>()
            {
                return Some(ip);
            }
        } else {
            warn!(
                "tailscale ip -4 failed: status={:?} stderr={}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    } else {
        warn!("tailscale コマンドを実行できませんでした（PATH に無い可能性）");
    }
    if let Ok(out) = std::process::Command::new("ipconfig").output() {
        let s = String::from_utf8_lossy(&out.stdout);
        for line in s.lines() {
            let line = line.trim();
            if line.starts_with("IPv4") || line.contains("IP Address") {
                if let Some(part) = line.split(':').nth(1) {
                    if let Ok(ip) = part.trim().parse::<Ipv4Addr>() {
                        if is_cgnat(ip) {
                            return Some(ip);
                        }
                    }
                }
            }
        }
    }
    None
}

pub(crate) fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0xC0) == 0x40
}

fn normalize_allowed_host(host: &str) -> Option<String> {
    let host = host.trim().trim_end_matches('.');
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(windows)]
fn local_hostname() -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};

    let mut size = 0;
    unsafe {
        let _ = GetComputerNameExW(ComputerNameDnsHostname, PWSTR::null(), &mut size);
    }
    if size == 0 {
        return None;
    }
    let mut buffer = vec![0_u16; size as usize];
    unsafe {
        GetComputerNameExW(
            ComputerNameDnsHostname,
            PWSTR::from_raw(buffer.as_mut_ptr()),
            &mut size,
        )
        .ok()
        .map(|()| String::from_utf16_lossy(&buffer[..size as usize]))
    }
}

#[cfg(not(windows))]
fn local_hostname() -> Option<String> {
    let mut buffer = [0_u8; 256];
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return None;
    }
    let len = buffer.iter().position(|byte| *byte == 0)?;
    std::str::from_utf8(&buffer[..len]).ok().map(str::to_owned)
}

fn tailscale_self_names() -> Vec<String> {
    let Ok(output) = std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output()
    else {
        warn!("tailscale status --json を実行できませんでした");
        return Vec::new();
    };
    if !output.status.success() {
        warn!(
            "tailscale status --json failed: status={:?} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return Vec::new();
    }
    let Ok(names) = tailscale_names_from_status(&output.stdout) else {
        warn!("tailscale status --json の解析に失敗しました");
        return Vec::new();
    };
    names
}

pub(crate) fn tailscale_names_from_status(json: &[u8]) -> serde_json::Result<Vec<String>> {
    let value = serde_json::from_slice::<serde_json::Value>(json)?;
    let Some(self_node) = value.get("Self") else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    if let Some(dns_name) = self_node.get("DNSName").and_then(|value| value.as_str()) {
        names.push(dns_name.to_string());
        if let Some(first_label) = dns_name.trim_end_matches('.').split('.').next() {
            names.push(first_label.to_string());
        }
    }
    if let Some(host_name) = self_node.get("HostName").and_then(|value| value.as_str()) {
        names.push(host_name.to_string());
    }
    Ok(names)
}

pub(crate) fn build_host_policy(
    mode: HostCheckMode,
    additional: impl IntoIterator<Item = String>,
) -> HostPolicy {
    let mut allowed =
        std::collections::BTreeSet::from(["127.0.0.1".to_string(), "localhost".to_string()]);
    if let Some(ip) = find_tailscale_ip() {
        allowed.insert(ip.to_string());
    }
    for host in local_hostname()
        .into_iter()
        .chain(tailscale_self_names())
        .chain(additional)
    {
        if let Some(host) = normalize_allowed_host(&host) {
            allowed.insert(host);
        }
    }
    HostPolicy {
        mode,
        allowed: Arc::new(allowed),
    }
}

// ── Auth (serve) ──────────────────────────────────────────────────────────────

pub(crate) fn check_auth(token: &Option<String>, headers: &HeaderMap) -> bool {
    use subtle::ConstantTimeEq;

    let Some(expected) = token else { return true };
    let Some(actual) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let expected = format!("Bearer {expected}");
    bool::from(actual.as_bytes().ct_eq(expected.as_bytes()))
}

pub(crate) fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "Unauthorized\n").into_response()
}

// ── HTTP handlers (serve) ─────────────────────────────────────────────────────

pub(crate) const PROTOCOL_VERSION: u32 = 2;
pub(crate) const PROTOCOL_FEATURES: &[&str] = &["hash", "timeout"];

pub(crate) async fn handle_health(headers: HeaderMap) -> Response {
    let wants_json = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|media| {
                media
                    .split(';')
                    .next()
                    .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
            })
        });
    if wants_json {
        return axum::Json(serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "proto": PROTOCOL_VERSION,
            "features": PROTOCOL_FEATURES,
        }))
        .into_response();
    }
    "OK\n".into_response()
}

pub(crate) fn resolve_serve_token(
    cli_token: Option<&str>,
    token_file: Option<&Path>,
    env_token: Option<&str>,
) -> Result<(Option<String>, bool)> {
    if let Some(token) = cli_token {
        return Ok((Some(token.to_string()), true));
    }
    if let Some(path) = token_file {
        let token = std::fs::read_to_string(path)
            .with_context(|| format!("token-file を読み込めません: {}", path.display()))?;
        let token = token
            .trim()
            .strip_prefix('\u{feff}')
            .unwrap_or(token.trim());
        let token = token.trim();
        if token.is_empty() {
            bail!("token-file が空です: {}", path.display());
        }
        return Ok((Some(token.to_string()), false));
    }
    Ok((env_token.map(str::to_string), false))
}

pub(crate) const CLI_TOKEN_DEPRECATION_WARNING: &str =
    "--token はプロセス一覧に露出するため非推奨です。--token-file を使用してください";
pub(crate) const AUTO_APPROVE_TOKEN_WARNING: &str =
    "serve-start auto_approve=true auth=token: トークン保有者は Windows ユーザー権限で任意のコードを承認なしで登録・実行できます";
pub(crate) const AUTO_APPROVE_NO_TOKEN_WARNING: &str =
    "DANGER serve-start auto_approve=true auth=none: 到達できるすべての者が Windows ユーザー権限で任意のコードを承認なしで登録・実行できます。Tailscale ACL で接続元を制限してください";

pub(crate) fn validate_serve_security(
    auto_approve: bool,
    token: &Option<String>,
    allow_no_token: bool,
) -> Result<()> {
    if auto_approve && token.is_none() && !allow_no_token {
        bail!("--auto-approve には --token-file <path> または --allow-no-token が必要です。");
    }
    Ok(())
}

pub(crate) fn warn_auto_approve(has_token: bool) {
    warn!(
        "{}",
        if has_token {
            AUTO_APPROVE_TOKEN_WARNING
        } else {
            AUTO_APPROVE_NO_TOKEN_WARNING
        }
    );
}

// ── serve entry point ─────────────────────────────────────────────────────────

pub(crate) async fn run_serve(args: ServeArgs) -> Result<()> {
    let env_token = std::env::var("CLIPD_TOKEN").ok();
    // `-WindowStyle Hidden` で起動すると標準出力/標準エラーがどこにも残らず、
    // tracing のログも panic メッセージも消える（「ログが何もない」の直接の
    // 原因）。ログファイルへ明示的に書く。`_guard` は非同期書き込みスレッドを
    // 生かし続けるためにこの関数のスタックで保持する（`serve_forever` が
    // 正常時は無限ループのため、事実上プロセス終了までドロップされない）。
    let log_path = log_file_path();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file_appender = tracing_appender::rolling::never(
        log_path.parent().unwrap_or_else(|| Path::new(".")),
        log_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("clipd.log")),
    );
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clipwire=info".into()),
        )
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();
    let (token, used_cli_token) = resolve_serve_token(
        args.token.as_deref(),
        args.token_file.as_deref(),
        env_token.as_deref(),
    )?;
    info!(
        "clipd starting (pid={}), log file: {}",
        std::process::id(),
        log_path.display()
    );
    if used_cli_token {
        warn!("{}", CLI_TOKEN_DEPRECATION_WARNING);
    }
    validate_serve_security(args.auto_approve, &token, args.allow_no_token)?;
    if args.auto_approve {
        warn_auto_approve(token.is_some());
    }

    if !args.bind_localhost_only && token.is_none() && !args.allow_no_token {
        bail!(
            "トークンなしでの起動を拒否しました。\n\
             --token-file <path>、CLIPD_TOKEN 環境変数、または --allow-no-token を指定してください。"
        );
    }

    let config_dir = clipwire_config_dir();

    #[cfg(windows)]
    let _mutex = unsafe { win_clip::acquire_mutex(&config_dir) };
    let _pid_file = PidFile::create(&config_dir)?;
    #[cfg(windows)]
    win_clip::ensure_aumid_registered();

    let (clip_tx, clip_rx) = mpsc::sync_channel::<ClipRequest>(32);
    thread::Builder::new()
        .name("clipboard-sta".into())
        .spawn(move || {
            #[cfg(windows)]
            win_clip::sta_loop(clip_rx);
            #[cfg(not(windows))]
            {
                drop(clip_rx);
            }
        })?;

    warn_invalid_stored_target_names(&config_dir);

    let store = Store::new(config_dir.clone());
    store
        .migrate()
        .await
        .context("承認ストアを移行できません")?;
    store
        .cleanup_approval_records(crate::store::APPROVAL_RETENTION, SystemTime::now())
        .await
        .context("承認レコードを掃除できません")?;
    let cleanup_store = store.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(crate::store::APPROVAL_CLEANUP_INTERVAL).await;
            match cleanup_store
                .cleanup_approval_records(crate::store::APPROVAL_RETENTION, SystemTime::now())
                .await
            {
                Ok(removed) if removed > 0 => {
                    tracing::info!(removed, "期限切れの承認レコードを削除しました");
                }
                Ok(_) => {}
                Err(error) => tracing::error!("承認レコードの定期掃除に失敗しました: {error:#}"),
            }
        }
    });
    let audit = AuditLog::new(config_dir.clone());
    let state = AppState {
        clip_tx,
        token,
        allow_no_token: args.allow_no_token,
        last_clip: Arc::new(Mutex::new(LastClip::default())),
        store,
        config_dir,
        auto_approve: args.auto_approve,
        host_policy: build_host_policy(args.host_check, args.allow_host),
        audit,
    };

    let serve_start = crate::audit::serve_start_event(
        crate::audit::audit_timestamp(SystemTime::now()),
        args.auto_approve,
        state.token.is_some(),
    );
    state.audit.record(serve_start);

    let app = build_router(state.clone());

    let localhost = SocketAddr::from(([127, 0, 0, 1], args.port));

    if args.bind_localhost_only {
        serve_until_shutdown(serve_forever(localhost, app, "localhost")).await;
    } else {
        match find_tailscale_ip() {
            Some(ts_ip) => {
                let ts_addr = SocketAddr::from((ts_ip, args.port));
                let app2 = app.clone();
                tokio::spawn(serve_forever(localhost, app2, "localhost"));
                serve_until_shutdown(serve_forever(ts_addr, app, "tailscale")).await;
            }
            None => {
                warn!("Tailscale IP not found; falling back to localhost-only");
                serve_until_shutdown(serve_forever(localhost, app, "localhost")).await;
            }
        }
    }
    Ok(())
}

async fn serve_until_shutdown(server: impl std::future::Future<Output = ()>) {
    tokio::select! {
        _ = server => {}
        signal = tokio::signal::ctrl_c() => match signal {
            Ok(()) => info!("shutdown signal received"),
            Err(error) => {
                warn!("shutdown signal handler failed: {error}; continuing to serve");
                std::future::pending::<()>().await;
            }
        }
    }
}

/// `addr` へ bind して `app` を serve し続ける。エラーが起きても**プロセスを
/// 道連れにせず**、ログを出して5秒後に再試行する（bind 自体の失敗、serve
/// ループ中の異常終了のいずれも同様）。
///
/// # 修正の背景（2026-08-15）
///
/// 旧実装は Tailscale 向けリスナーだけ `axum::serve(..).await?` で
/// `run_serve` → `main()` までエラーを伝播させ、**プロセスごと終了**していた
/// （localhost 向けリスナーは対称的に `.ok()` で握り潰していたのに、
/// こちらだけ非対称だった）。Tailscale の接続は relay⇔direct の切替や
/// 一時的な `offline` 状態を伴うことがあり、そのタイミングで
/// `TcpListener::bind`/`axum::serve` がエラーを返すと、そのままサーバー
/// プロセス全体が落ちていた（実機で「何度再起動しても落ちる」として再現）。
/// この関数に一本化し、どのリスナーも「失敗したら再試行し続ける」対称な
/// 挙動にした。
pub(crate) async fn serve_forever(addr: SocketAddr, app: Router, label: &str) {
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                info!("Listening on http://{addr} ({label})");
                if let Err(e) = axum::serve(
                    listener,
                    app.clone()
                        .into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                {
                    warn!("{label} リスナー ({addr}) が停止しました: {e}。5秒後に再試行します");
                }
            }
            Err(e) => {
                warn!("{label} ({addr}) への bind に失敗しました: {e}。5秒後に再試行します");
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ── main ──────────────────────────────────────────────────────────────────────
