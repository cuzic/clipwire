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

// ── Exec target config ────────────────────────────────────────────────────────

/// `steps` フィールドの値: 構造化配列 or 1行1コマンドの文字列
#[derive(serde::Deserialize, serde::Serialize, Clone)]
#[serde(untagged)]
enum StepsDef {
    Text(String),
    Argv(Vec<Vec<String>>),
}

impl StepsDef {
    fn into_argv(self) -> Vec<Vec<String>> {
        match self {
            StepsDef::Argv(v) => v,
            StepsDef::Text(s) => s
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .filter_map(shlex::split)
                .collect(),
        }
    }
}

/// Linux 側 targets.toml のエントリ兼 HTTP 登録ペイロード (dir なし)
#[derive(serde::Deserialize, serde::Serialize, Clone)]
#[serde(untagged)]
enum ExecPayload {
    Script {
        script: String,
    },
    Steps {
        steps: StepsDef,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
    },
}

/// Windows 側 pending.toml / registered.toml のエントリ (dir あり)
#[derive(serde::Deserialize, serde::Serialize, Clone, Default)]
struct StoredTarget {
    #[serde(skip_serializing_if = "Option::is_none")]
    dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    steps: Option<StepsDef>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    env: std::collections::HashMap<String, String>,
}

impl StoredTarget {
    fn into_exec(self) -> Result<(Option<String>, ExecPayload)> {
        if let Some(s) = self.script {
            Ok((self.dir, ExecPayload::Script { script: s }))
        } else if let Some(steps) = self.steps {
            Ok((
                self.dir,
                ExecPayload::Steps {
                    steps,
                    env: self.env,
                },
            ))
        } else {
            bail!("ターゲットに script も steps もありません")
        }
    }
}

type TargetMap = std::collections::HashMap<String, StoredTarget>;

fn clipwire_config_dir() -> PathBuf {
    dirs_next::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("clipwire")
}

fn load_target_map(path: &Path) -> Result<TargetMap> {
    if !path.exists() {
        return Ok(TargetMap::default());
    }
    Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
}

/// `load_target_map` を呼び、失敗（ファイルは存在するが読めない/壊れている）
/// 場合は空マップにフォールバックしつつ**必ず警告ログを残す**。
/// 黙って空マップ扱いにすると、破損に気づかないまま
/// 「登録した全ターゲットが消えた」ように見えてしまう
/// （サーバー側呼び出し元専用。`unwrap_or_default()` を直接使わないこと）。
fn load_target_map_or_warn(path: &Path) -> TargetMap {
    match load_target_map(path) {
        Ok(m) => m,
        Err(e) => {
            warn!(
                "{} の読み込みに失敗しました（空として扱います）: {e:#}",
                path.display()
            );
            TargetMap::default()
        }
    }
}

fn save_target_map(path: &Path, map: &TargetMap) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, toml::to_string(map)?)?;
    Ok(())
}

#[derive(serde::Deserialize)]
struct TargetsFile {
    targets: std::collections::HashMap<String, StoredTarget>,
}

fn load_exec_target(name: &str) -> Result<StoredTarget> {
    let path = clipwire_config_dir().join("targets.toml");
    let src = std::fs::read_to_string(&path)
        .with_context(|| format!("設定ファイルが見つかりません: {}", path.display()))?;
    let file: TargetsFile = toml::from_str(&src)?;
    file.targets
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
        .with_context(|| format!("ターゲット '{}' が定義されていません", name))
}

fn validate_target_name(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let valid_first = bytes.first().is_some_and(|b| b.is_ascii_alphanumeric());
    let valid_rest = bytes
        .iter()
        .skip(1)
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if bytes.len() <= 64 && valid_first && valid_rest && !name.contains("..") {
        Ok(())
    } else {
        bail!("無効なターゲット名です: ASCII英数字で始まる1〜64文字の英数字・'.'・'_'・'-'を指定してください ('..' は使用不可)")
    }
}

fn invalid_stored_target_names(config_dir: &Path) -> Vec<(PathBuf, String)> {
    ["registered.toml", "pending.toml"]
        .into_iter()
        .filter_map(|file| {
            let path = config_dir.join(file);
            load_target_map(&path).ok().map(|map| (path, map))
        })
        .flat_map(|(path, map)| {
            map.into_keys()
                .filter(|name| validate_target_name(name).is_err())
                .map(move |name| (path.clone(), name))
        })
        .collect()
}

fn warn_invalid_stored_target_names(config_dir: &Path) {
    for (path, name) in invalid_stored_target_names(config_dir) {
        warn!(
            "{} に無効なターゲット名が存在します: {:?}",
            path.display(),
            name
        );
    }
}

// Production callers are Windows-only; Linux keeps this available for L tests.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\u{9}' | '\u{a}' | '\u{d}')
            || ('\u{20}'..='\u{d7ff}').contains(&ch)
            || ('\u{e000}'..='\u{fffd}').contains(&ch)
            || ('\u{10000}'..='\u{10ffff}').contains(&ch)
        {
            match ch {
                '&' => escaped.push_str("&amp;"),
                '<' => escaped.push_str("&lt;"),
                '>' => escaped.push_str("&gt;"),
                '"' => escaped.push_str("&quot;"),
                '\'' => escaped.push_str("&apos;"),
                _ => escaped.push(ch),
            }
        }
    }
    escaped
}

// ── Client config ─────────────────────────────────────────────────────────────

struct ClientConfig {
    host: String,
    port: u16,
    token: Option<String>,
}

impl ClientConfig {
    fn from_env() -> Result<Self> {
        let host = std::env::var("CLIPD_HOST")
            .context("CLIPD_HOST が設定されていません (例: export CLIPD_HOST=my-windows)")?;
        let port = std::env::var("CLIPD_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9999u16);
        let token = std::env::var("CLIPD_TOKEN").ok();
        Ok(Self { host, port, token })
    }

    fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    fn set_auth(&self, req: ureq::Request) -> ureq::Request {
        if let Some(token) = &self.token {
            req.set("Authorization", &format!("Bearer {}", token))
        } else {
            req
        }
    }
}

// ── Client: get ───────────────────────────────────────────────────────────────

fn cmd_get(cfg: &ClientConfig, args: &GetArgs) -> Result<()> {
    let url = format!("{}/clip", cfg.base_url());
    let req = cfg.set_auth(ureq::get(&url).timeout(Duration::from_secs(10)));

    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(401, _)) => bail!("Unauthorized (CLIPD_TOKEN を確認)"),
        Err(ureq::Error::Status(code, r)) => {
            bail!(
                "HTTP {}: {}",
                code,
                r.into_string().unwrap_or_default().trim()
            )
        }
        Err(e) => bail!("{} に接続できません: {}", cfg.base_url(), e),
    };

    let kind = resp.header("X-Clip-Kind").unwrap_or("").to_string();
    let mut body = Vec::new();
    resp.into_reader().read_to_end(&mut body)?;

    match kind.as_str() {
        "image" => {
            let path = save_file(&body, ".png", args.dir.as_deref())?;
            if args.quiet {
                println!("{}", path.display());
            } else {
                println!("画像を保存しました: {}", path.display());
            }
        }

        "text" | "html" | "rtf" => {
            let (suffix, label) = match kind.as_str() {
                "html" => (".html", "HTML"),
                "rtf" => (".rtf", "RTF"),
                _ => (".txt", "テキスト"),
            };
            if body.len() > 1024 {
                let path = save_file(&body, suffix, args.dir.as_deref())?;
                if args.quiet {
                    println!("{}", path.display());
                } else {
                    println!("{} を保存しました (大容量): {}", label, path.display());
                }
            } else {
                io::stdout().write_all(&body)?;
                if !args.quiet {
                    println!();
                }
            }
        }

        "url" => {
            io::stdout().write_all(&body)?;
            if !args.quiet {
                println!();
            }
        }

        "audio" => {
            let path = save_file(&body, ".wav", args.dir.as_deref())?;
            if args.quiet {
                println!("{}", path.display());
            } else {
                println!("音声を保存しました: {}", path.display());
            }
        }

        "files" => {
            let paths: Vec<String> = serde_json::from_slice(&body)?;
            if paths.is_empty() {
                if !args.quiet {
                    println!("クリップボードにファイルがありません。");
                }
                return Ok(());
            }
            let auth = curl_auth_argument(cfg.token.is_some());
            if !args.quiet {
                println!("クリップボード: ファイル {}件\n", paths.len());
            }
            for win_path in &paths {
                let enc = percent_encode(win_path);
                let fname = win_fname(win_path);
                let cmd = download_command(
                    &format!("{}/file?path={}", cfg.base_url(), enc),
                    &fname,
                    !auth.is_empty(),
                )?;
                if args.quiet {
                    println!("{cmd}");
                } else {
                    println!("  {win_path}\n  → {cmd}\n");
                }
            }
        }

        "vfiles" => {
            let names: Vec<String> = serde_json::from_slice(&body)?;
            if names.is_empty() {
                if !args.quiet {
                    println!("仮想ファイルがありません。");
                }
                return Ok(());
            }
            let auth = curl_auth_argument(cfg.token.is_some());
            if !args.quiet {
                println!(
                    "クリップボード: 仮想ファイル {}件 (Outlook 添付等)\n",
                    names.len()
                );
            }
            for (i, name) in names.iter().enumerate() {
                let cmd = download_command(
                    &format!("{}/vfile?i={}", cfg.base_url(), i),
                    name,
                    !auth.is_empty(),
                )?;
                if args.quiet {
                    println!("{cmd}");
                } else {
                    println!("  [{i}] {name}\n  → {cmd}\n");
                }
            }
        }

        "empty" | "" => { /* サイレント */ }

        "error" => bail!("clipwire error: {}", String::from_utf8_lossy(&body).trim()),

        other => {
            eprintln!("unknown kind: {other}");
            io::stdout().write_all(&body)?;
        }
    }

    Ok(())
}

// ── Client: put ───────────────────────────────────────────────────────────────

fn cmd_put(cfg: &ClientConfig) -> Result<()> {
    let mut body = Vec::new();
    io::stdin().read_to_end(&mut body)?;

    let url = format!("{}/clip", cfg.base_url());
    let req = cfg.set_auth(
        ureq::post(&url)
            .set("Content-Type", "text/plain; charset=utf-8")
            .timeout(Duration::from_secs(30)),
    );

    match req.send_bytes(&body) {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(401, _)) => bail!("Unauthorized (CLIPD_TOKEN を確認)"),
        Err(ureq::Error::Status(code, r)) => {
            bail!(
                "HTTP {}: {}",
                code,
                r.into_string().unwrap_or_default().trim()
            )
        }
        Err(e) => bail!("{} への送信に失敗: {}", cfg.base_url(), e),
    }
}

// ── Client: exec ──────────────────────────────────────────────────────────────

fn cmd_exec(cfg: &ClientConfig, args: &ExecArgs) -> Result<()> {
    validate_target_name(&args.target)?;
    let body = serde_json::json!({ "name": args.target }).to_string();
    let url = format!("{}/exec", cfg.base_url());
    let req = cfg.set_auth(
        ureq::post(&url)
            .set("Content-Type", "application/json")
            .timeout(Duration::from_secs(600)),
    );
    let resp = match req.send_string(&body) {
        Ok(r) => r,
        Err(ureq::Error::Status(401, _)) => bail!("Unauthorized (CLIPD_TOKEN を確認)"),
        Err(ureq::Error::Status(404, _)) => bail!(
            "'{}' は Windows 側に登録されていません。先に clipwire register を実行してください",
            args.target
        ),
        Err(ureq::Error::Status(409, r)) => bail!("{}", r.into_string().unwrap_or_default().trim()),
        Err(ureq::Error::Status(503, r)) => bail!("{}", r.into_string().unwrap_or_default().trim()),
        Err(e) => bail!("{} に接続できません: {}", cfg.base_url(), e),
    };
    let exit_code: i32 = resp
        .header("X-Exit-Code")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let output = resp.into_string()?;
    print!("{output}");
    if exit_code != 0 {
        bail!("exit code {exit_code}");
    }
    Ok(())
}

// ── Client: register ──────────────────────────────────────────────────────────

fn cmd_register(cfg: &ClientConfig, args: &RegisterArgs) -> Result<()> {
    validate_target_name(&args.target)?;
    let target = load_exec_target(&args.target)?;
    let mut body = serde_json::to_value(&target)?;
    let obj = body.as_object_mut().unwrap();
    obj.insert("name".into(), args.target.clone().into());
    let url = format!("{}/register", cfg.base_url());
    let req = cfg.set_auth(
        ureq::post(&url)
            .set("Content-Type", "application/json")
            .timeout(Duration::from_secs(30)),
    );
    match req.send_string(&body.to_string()) {
        Ok(r) => {
            print!("{}", r.into_string().unwrap_or_default());
            Ok(())
        }
        Err(ureq::Error::Status(401, _)) => bail!("Unauthorized (CLIPD_TOKEN を確認)"),
        Err(e) => bail!("{} に接続できません: {}", cfg.base_url(), e),
    }
}

// ── Local: approve (Windows 側で実行) ────────────────────────────────────────

fn cmd_approve(args: &ApproveArgs) -> Result<()> {
    let config_dir = clipwire_config_dir();
    let pending_path = config_dir.join("pending.toml");
    let mut pending = load_target_map(&pending_path)?;

    if args.target.is_none() {
        if pending.is_empty() {
            println!("承認待ちのターゲットはありません");
        } else {
            for name in pending.keys() {
                println!("{name}");
            }
        }
        return Ok(());
    }

    let name = args.target.as_ref().unwrap();
    let mut entry = pending
        .remove(name)
        .with_context(|| format!("'{}' は pending にありません", name))?;

    if let Some(ref d) = args.dir {
        entry.dir = Some(d.clone());
    }

    // 承認内容を表示
    println!("=== {} ===", name);
    if let Some(ref d) = entry.dir {
        println!("dir:    {}", d);
    }
    if let Some(ref s) = entry.script {
        println!("script:\n{}", s.trim());
    } else if let Some(ref steps) = entry.steps {
        let lines = match steps {
            StepsDef::Text(s) => s.trim().to_string(),
            StepsDef::Argv(v) => v.iter().map(|a| a.join(" ")).collect::<Vec<_>>().join("\n"),
        };
        println!("steps:\n{}", lines);
    }

    let registered_path = config_dir.join("registered.toml");
    let mut registered = load_target_map(&registered_path).unwrap_or_default();
    registered.insert(name.clone(), entry);
    save_target_map(&registered_path, &registered)?;
    save_target_map(&pending_path, &pending)?;
    println!("承認しました");
    Ok(())
}

// ── Client: open ──────────────────────────────────────────────────────────────

fn cmd_open(cfg: &ClientConfig, args: &OpenArgs) -> Result<()> {
    let url = format!("{}/open?name={}", cfg.base_url(), args.target.as_str());
    let req = cfg.set_auth(ureq::get(&url).timeout(Duration::from_secs(10)));
    match req.call() {
        Ok(_) => {
            println!("Windows ブラウザで {} を開きました", args.target.url());
            Ok(())
        }
        Err(ureq::Error::Status(401, _)) => bail!("Unauthorized (CLIPD_TOKEN を確認)"),
        Err(ureq::Error::Status(code, r)) => {
            bail!(
                "HTTP {}: {}",
                code,
                r.into_string().unwrap_or_default().trim()
            )
        }
        Err(e) => bail!("{} に接続できません: {}", cfg.base_url(), e),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn save_file(data: &[u8], suffix: &str, dir: Option<&Path>) -> Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let name = format!("clipwire_{}{}", ts, suffix);
    let path = match dir {
        Some(d) => {
            std::fs::create_dir_all(d)?;
            d.join(&name)
        }
        None => std::env::temp_dir().join(&name),
    };
    std::fs::write(&path, data)?;
    Ok(path)
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn win_fname(win_path: &str) -> String {
    let norm = win_path.replace('\\', "/");
    Path::new(&norm)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string())
}

fn curl_auth_argument(has_token: bool) -> &'static str {
    if has_token {
        "-H \"Authorization: Bearer $CLIPD_TOKEN\" "
    } else {
        ""
    }
}

fn safe_download_name(output: &str) -> String {
    let normalized = output.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or_default();
    if name.is_empty() || matches!(name, "." | "..") || name.chars().any(char::is_control) {
        "file".to_string()
    } else {
        name.to_string()
    }
}

fn download_command(url: &str, output: &str, has_token: bool) -> Result<String> {
    let url = shlex::try_quote(url).context("download URL をシェル引用できません")?;
    let output = safe_download_name(output);
    let output = shlex::try_quote(&output).context("出力ファイル名をシェル引用できません")?;
    Ok(format!(
        "curl -fsSL {}{} -o {}",
        curl_auth_argument(has_token),
        url,
        output
    ))
}

// ── Domain types (serve) ──────────────────────────────────────────────────────

#[derive(Debug)]
#[allow(dead_code)]
enum ClipKind {
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
enum ClipRequest {
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
struct LastClip {
    files: Vec<String>,
    vfiles: Vec<String>,
}

#[derive(Clone)]
struct AppState {
    clip_tx: mpsc::SyncSender<ClipRequest>,
    token: Option<String>,
    last_clip: Arc<Mutex<LastClip>>,
    config_dir: PathBuf,
    auto_approve: bool,
}

// ── Windows clipboard implementation ──────────────────────────────────────────

#[cfg(windows)]
mod win_clip {
    use super::ClipKind;
    use anyhow::{bail, Context, Result};
    use windows::{
        core::w,
        Win32::{
            Foundation::{GetLastError, HANDLE, HGLOBAL, WIN32_ERROR},
            System::{
                Com::{
                    CoInitializeEx, CoUninitialize, IStream, COINIT_APARTMENTTHREADED,
                    COINIT_MULTITHREADED, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL,
                    TYMED_ISTREAM,
                },
                DataExchange::{
                    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
                    OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
                },
                Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
                Ole::{OleGetClipboard, OleInitialize, ReleaseStgMedium},
                Threading::CreateMutexW,
            },
            UI::Shell::{DragQueryFileW, HDROP},
        },
    };

    const CF_DIB: u32 = 8;
    const CF_WAVE: u32 = 12;
    const CF_UNICODETEXT: u32 = 13;
    const CF_HDROP: u32 = 15;

    struct ClipGuard;
    impl ClipGuard {
        fn open() -> Result<Self> {
            unsafe { OpenClipboard(None)? };
            Ok(ClipGuard)
        }
    }
    impl Drop for ClipGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    unsafe fn fmt_avail(fmt: u32) -> bool {
        IsClipboardFormatAvailable(fmt).is_ok()
    }

    unsafe fn reg_fmt(name: windows::core::PCWSTR) -> u32 {
        RegisterClipboardFormatW(name)
    }

    /// `GlobalLock` が失敗（null 返却）した場合、null ポインタから
    /// `from_raw_parts` するのは未定義動作（Rust の panic にはならず、
    /// catch_unwind でも panic hook でも捕まえられない segfault 相当の
    /// クラッシュになりうる）。ここで明示的に弾く。
    unsafe fn hglobal_bytes(h: HANDLE) -> Vec<u8> {
        let hg = HGLOBAL(h.0);
        let size = GlobalSize(hg);
        let ptr = GlobalLock(hg);
        if ptr.is_null() {
            tracing::warn!("hglobal_bytes: GlobalLock が null を返しました (size={size})");
            return Vec::new();
        }
        let data = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
        let _ = GlobalUnlock(hg);
        data
    }

    pub unsafe fn read_clipboard() -> ClipKind {
        let fmt_fgd = reg_fmt(w!("FileGroupDescriptorW"));
        let fmt_fc = reg_fmt(w!("FileContents"));
        let fmt_html = reg_fmt(w!("HTML Format"));
        let fmt_url = reg_fmt(w!("UniformResourceLocatorW"));
        let fmt_rtf = reg_fmt(w!("Rich Text Format"));

        if fmt_avail(CF_DIB) {
            if let Ok(d) = read_image() {
                return ClipKind::Image(d);
            }
        }
        if fmt_avail(CF_HDROP) {
            if let Ok(v) = read_files() {
                if !v.is_empty() {
                    return ClipKind::Files(v);
                }
            }
        }
        if fmt_avail(fmt_fgd) && fmt_avail(fmt_fc) {
            if let Ok(v) = read_vfile_names() {
                if !v.is_empty() {
                    return ClipKind::VFiles(v);
                }
            }
        }
        if fmt_avail(CF_WAVE) {
            if let Ok(d) = read_wave() {
                return ClipKind::Audio(d);
            }
        }
        if fmt_avail(fmt_url) {
            if let Ok(u) = read_url(fmt_url) {
                return ClipKind::Url(u);
            }
        }
        if fmt_avail(fmt_html) {
            if let Ok(h) = read_html(fmt_html) {
                return ClipKind::Html(h);
            }
        }
        if fmt_avail(fmt_rtf) {
            if let Ok(r) = read_rtf(fmt_rtf) {
                return ClipKind::Rtf(r);
            }
        }
        if fmt_avail(CF_UNICODETEXT) {
            if let Ok(t) = read_text() {
                if !t.is_empty() {
                    return ClipKind::Text(t);
                }
            }
        }
        ClipKind::Empty
    }

    unsafe fn read_image() -> Result<Vec<u8>> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(CF_DIB).context("CF_DIB")?;
        dib_to_png(&hglobal_bytes(h))
    }

    fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
        if dib.len() < 40 {
            bail!("DIB too short");
        }
        let info_size = u32::from_le_bytes(dib[0..4].try_into()?) as usize;
        let bpp = u16::from_le_bytes(dib[14..16].try_into()?) as usize;
        let clr_count = if bpp <= 8 { 1usize << bpp } else { 0 };
        let pix_offset = 14 + info_size + clr_count * 4;
        let file_size = 14 + dib.len();

        let mut bmp = Vec::with_capacity(file_size);
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(file_size as u32).to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&(pix_offset as u32).to_le_bytes());
        bmp.extend_from_slice(dib);

        let img = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp)?;
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
        Ok(png)
    }

    unsafe fn read_files() -> Result<Vec<String>> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(CF_HDROP).context("CF_HDROP")?;
        let hdrop = HDROP(h.0);
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        let mut paths = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            paths.push(String::from_utf16_lossy(&buf[..len]));
        }
        Ok(paths)
    }

    unsafe fn read_vfile_names() -> Result<Vec<String>> {
        let fmt = reg_fmt(w!("FileGroupDescriptorW"));
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(fmt).context("FileGroupDescriptorW")?;
        parse_fgd(&hglobal_bytes(h))
    }

    fn parse_fgd(data: &[u8]) -> Result<Vec<String>> {
        if data.len() < 4 {
            bail!("FGD too short");
        }
        let count = u32::from_le_bytes(data[0..4].try_into()?) as usize;
        const ENTRY: usize = 592;
        const NAME: usize = 72;
        let mut names = Vec::with_capacity(count);
        for i in 0..count {
            let base = 4 + i * ENTRY;
            if base + ENTRY > data.len() {
                break;
            }
            let wdata: &[u8] = &data[base + NAME..base + NAME + 520];
            let words: Vec<u16> = wdata
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            let end = words.iter().position(|&w| w == 0).unwrap_or(words.len());
            names.push(String::from_utf16_lossy(&words[..end]));
        }
        Ok(names)
    }

    pub unsafe fn get_vfile_contents(index: usize) -> Result<Vec<u8>> {
        let fmt_fc = reg_fmt(w!("FileContents"));
        let _ = OleInitialize(None);

        let data_obj = OleGetClipboard()?;

        let mut fetc = FORMATETC {
            cfFormat: fmt_fc as u16,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: index as i32,
            tymed: TYMED_ISTREAM.0 as u32,
        };

        if let Ok(mut stgm) = data_obj.GetData(&fetc) {
            if stgm.tymed == TYMED_ISTREAM.0 as u32 {
                let data = {
                    let stream: &Option<IStream> = &stgm.u.pstm;
                    if let Some(s) = stream {
                        drain_istream(s)?
                    } else {
                        bail!("null IStream")
                    }
                };
                ReleaseStgMedium(&mut stgm);
                return Ok(data);
            }
            ReleaseStgMedium(&mut stgm);
        }

        fetc.tymed = TYMED_HGLOBAL.0 as u32;
        let mut stgm = data_obj.GetData(&fetc)?;
        let hg = stgm.u.hGlobal;
        let size = GlobalSize(hg);
        let ptr = GlobalLock(hg);
        if ptr.is_null() {
            ReleaseStgMedium(&mut stgm);
            bail!("get_vfile_contents: GlobalLock が null を返しました (size={size})");
        }
        let data = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
        let _ = GlobalUnlock(hg);
        ReleaseStgMedium(&mut stgm);
        Ok(data)
    }

    unsafe fn drain_istream(stream: &IStream) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            let mut read = 0u32;
            let _ = stream.Read(
                chunk.as_mut_ptr() as *mut _,
                chunk.len() as u32,
                Some(&mut read),
            );
            if read == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..read as usize]);
        }
        Ok(buf)
    }

    unsafe fn read_wave() -> Result<Vec<u8>> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(CF_WAVE).context("CF_WAVE")?;
        Ok(hglobal_bytes(h))
    }

    unsafe fn read_html(fmt: u32) -> Result<String> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(fmt).context("HTML Format")?;
        let data = hglobal_bytes(h);
        let hdr = String::from_utf8_lossy(&data[..data.len().min(512)]);
        let start = hdr
            .lines()
            .find(|l| l.starts_with("StartHTML:"))
            .and_then(|l| l[10..].trim().parse::<usize>().ok())
            .unwrap_or(0);
        Ok(String::from_utf8_lossy(if start < data.len() {
            &data[start..]
        } else {
            &data
        })
        .into_owned())
    }

    unsafe fn read_url(fmt: u32) -> Result<String> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(fmt).context("URL format")?;
        let data = hglobal_bytes(h);
        let words: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let end = words.iter().position(|&w| w == 0).unwrap_or(words.len());
        Ok(String::from_utf16_lossy(&words[..end]).trim().to_string())
    }

    unsafe fn read_rtf(fmt: u32) -> Result<String> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(fmt).context("RTF")?;
        Ok(String::from_utf8_lossy(&hglobal_bytes(h)).into_owned())
    }

    unsafe fn read_text() -> Result<String> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(CF_UNICODETEXT).context("CF_UNICODETEXT")?;
        let data = hglobal_bytes(h);
        let words: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let end = words.iter().position(|&w| w == 0).unwrap_or(words.len());
        Ok(String::from_utf16_lossy(&words[..end]))
    }

    pub unsafe fn write_clipboard_text(text: &str) -> Result<()> {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let byte_len = wide.len() * 2;

        let hg = GlobalAlloc(GMEM_MOVEABLE, byte_len)?;
        let ptr = GlobalLock(hg) as *mut u16;
        if ptr.is_null() {
            bail!("GlobalLock failed");
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(hg);

        OpenClipboard(None)?;
        EmptyClipboard()?;
        if let Err(e) = SetClipboardData(CF_UNICODETEXT, HANDLE(hg.0)) {
            let _ = CloseClipboard();
            bail!("SetClipboardData: {e}");
        }
        CloseClipboard()?;
        Ok(())
    }

    pub fn show_balloon(msg: &str) {
        let msg = msg.to_string();
        std::thread::spawn(move || {
            if let Err(e) = show_toast(&msg) {
                let error = format!("show_balloon: toast の表示に失敗しました: {e}\n");
                tracing::error!("{}", error.trim());
                let _ = append_toast_log(&error);
            }
        });
    }

    fn append_toast_log(msg: &str) -> Result<()> {
        let path = super::clipwire_config_dir().join("toast.log");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(msg.as_bytes())?;
        Ok(())
    }

    fn show_toast(msg: &str) -> Result<()> {
        use windows::{
            core::HSTRING,
            Data::Xml::Dom::XmlDocument,
            UI::Notifications::{ToastNotification, ToastNotificationManager},
        };

        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        let xml_str = format!(
            r#"<toast><visual><binding template="ToastGeneric"><text>clipwire</text><text>{}</text></binding></visual></toast>"#,
            super::xml_escape(msg),
        );
        let xml = XmlDocument::new()?;
        xml.LoadXml(&HSTRING::from(xml_str))?;
        let toast = ToastNotification::CreateToastNotification(&xml)?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(CLIPWIRE_AUMID))?
            .Show(&toast)?;
        unsafe { CoUninitialize() };
        Ok(())
    }

    /// `register` 要求到着時に WinRT toast（承認/拒否ボタン付き）を表示する。
    /// 「承認」クリック → 同プロセス内の Activated ハンドラが registered.toml に直接書き込む。
    pub fn show_register_toast(
        name: String,
        entry: super::StoredTarget,
        config_dir: std::path::PathBuf,
        reapproval: bool,
    ) {
        let log_path = config_dir.join("toast.log");
        std::thread::spawn(move || {
            if let Err(e) =
                show_register_toast_impl(name.clone(), entry, config_dir.clone(), reapproval)
            {
                let msg = format!(
                    "[clipwire] register toast error for '{}': {e}; run `clipwire approve {}` to approve manually\n",
                    name, name
                );
                eprintln!("{}", msg.trim());
                let _ = std::fs::write(&log_path, &msg);
                tracing::error!("{}", msg.trim());
            }
        });
    }

    fn show_register_toast_impl(
        name: String,
        entry: super::StoredTarget,
        config_dir: std::path::PathBuf,
        reapproval: bool,
    ) -> Result<()> {
        // 実行確認用: 関数が呼ばれたら必ずログに記録する
        let log_path = config_dir.join("toast.log");
        let _ = std::fs::write(
            &log_path,
            format!(
                "[clipwire] show_register_toast_impl called for '{}'\n",
                name
            ),
        );

        use windows::{
            core::{Interface, HSTRING},
            Data::Xml::Dom::XmlDocument,
            Foundation::TypedEventHandler,
            UI::Notifications::{
                ToastActivatedEventArgs, ToastDismissedEventArgs, ToastNotification,
                ToastNotificationManager,
            },
        };

        // MTA で WinRT を初期化
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }

        let body = if reapproval {
            format!("'{}' の設定が変更されました。再承認しますか？", name)
        } else {
            format!("'{}' を承認しますか？", name)
        };
        let xml_str = format!(
            r#"<toast><visual><binding template="ToastGeneric"><text>clipwire: 登録要求</text><text>{}</text></binding></visual><actions><action content="承認" arguments="approve"/><action content="拒否" arguments="deny"/></actions></toast>"#,
            super::xml_escape(&body),
        );

        let xml = XmlDocument::new()?;
        xml.LoadXml(&HSTRING::from(xml_str.as_str()))?;
        let toast = ToastNotification::CreateToastNotification(&xml)?;

        let (tx, rx) = std::sync::mpsc::channel::<bool>();

        {
            let tx = tx.clone();
            toast.Activated(&TypedEventHandler::<
                ToastNotification,
                windows::core::IInspectable,
            >::new(move |_, args| {
                let approved = args
                    .as_ref()
                    .and_then(|a| a.cast::<ToastActivatedEventArgs>().ok())
                    .and_then(|a| a.Arguments().ok())
                    .map(|s| s == "approve")
                    .unwrap_or(false);
                let _ = tx.send(approved);
                Ok(())
            }))?;
        }
        {
            let tx = tx.clone();
            toast.Dismissed(&TypedEventHandler::<
                ToastNotification,
                ToastDismissedEventArgs,
            >::new(move |_, _| {
                let _ = tx.send(false);
                Ok(())
            }))?;
        }

        // スタートメニューのショートカットで登録済みの AUMID を使う
        let notifier =
            ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(CLIPWIRE_AUMID))?;
        let _ = std::fs::write(
            &log_path,
            format!("[clipwire] calling notifier.Show() for '{}'\n", name),
        );
        notifier.Show(&toast)?;
        let _ = std::fs::write(
            &log_path,
            format!("[clipwire] notifier.Show() succeeded for '{}'\n", name),
        );

        // ユーザー操作を最大 10 分待機
        if rx
            .recv_timeout(std::time::Duration::from_secs(600))
            .unwrap_or(false)
        {
            let registered_path = config_dir.join("registered.toml");
            let pending_path = config_dir.join("pending.toml");
            let mut registered = super::load_target_map_or_warn(&registered_path);
            let mut pending = super::load_target_map_or_warn(&pending_path);
            registered.insert(name.clone(), entry);
            pending.remove(&name);
            super::save_target_map(&registered_path, &registered)?;
            super::save_target_map(&pending_path, &pending)?;
            eprintln!("[clipwire] '{}' 承認 → registered.toml", name);
            show_balloon(&format!("'{}' を承認しました", name));
        }

        unsafe {
            CoUninitialize();
        }
        Ok(())
    }

    const CLIPWIRE_AUMID: &str = "cuzic.clipwire";

    /// サーバー起動時に一度だけ呼ぶ。
    /// プロセスに AUMID を設定することで CreateToastNotifierWithId が使えるようになる。
    pub fn ensure_aumid_registered() {
        unsafe {
            if let Err(e) = windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(
                windows::core::w!("cuzic.clipwire"),
            ) {
                eprintln!("[clipwire] SetCurrentProcessExplicitAppUserModelID failed: {e}");
            }
        }
    }

    /// `-WindowStyle Hidden` では eprintln! はどこにも表示されないため、直接
    /// ログファイルへ書く。`tracing::error!` は非同期（non-blocking writer）
    /// で実際の書き込みはバックグラウンドスレッドが行うため、直後の
    /// `std::process::exit`（デストラクタを一切走らせない）と組み合わせると
    /// メッセージが失われうる——ここでは同期的にファイルへ書いてから終了する。
    fn log_and_exit(msg: &str) -> ! {
        eprintln!("{msg}");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(super::log_file_path())
        {
            use std::io::Write as _;
            let _ = writeln!(f, "{msg}");
        }
        std::process::exit(1);
    }

    pub unsafe fn acquire_mutex() -> windows::Win32::Foundation::HANDLE {
        match CreateMutexW(None, true, w!("Global\\clipwire_singleton")) {
            Ok(h) => {
                if GetLastError() == WIN32_ERROR(183) {
                    log_and_exit("clipwire serve は既に起動中です。");
                }
                h
            }
            Err(e) => log_and_exit(&format!("CreateMutexW failed: {e}")),
        }
    }

    pub fn sta_loop(rx: std::sync::mpsc::Receiver<super::ClipRequest>) {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            use super::ClipRequest;
            for req in rx {
                match req {
                    ClipRequest::GetClip { reply } => {
                        let _ = reply.send(read_clipboard());
                    }
                    ClipRequest::SetClip { text, reply } => {
                        let _ = reply.send(write_clipboard_text(&text));
                    }
                    ClipRequest::GetFile { path, reply } => {
                        let _ = reply.send(std::fs::read(&path).ok());
                    }
                    ClipRequest::GetVFile { index, reply } => {
                        let _ = reply.send(get_vfile_contents(index).ok());
                    }
                }
            }
            CoUninitialize();
        }
    }
}

// ── Logging / panic visibility (serve) ────────────────────────────────────────

/// ログファイルのパス（`clipwire_config_dir()/clipd.log`）。
fn log_file_path() -> PathBuf {
    clipwire_config_dir().join("clipd.log")
}

/// panic 発生時にメッセージをログファイルへ直接追記する panic hook を設定する。
/// tokio ランタイム起動前、`main()` の一番最初で一度だけ呼ぶこと
/// （STA クリップボードスレッド等、tracing の非同期書き込みタスクとは別の
/// スレッドで起きた panic も、tracing の初期化タイミングに関係なく捕まえる
/// ため、tracing_subscriber::fmt().init() より前に独立して動く必要がある）。
///
/// これは Rust の catch_unwind 可能な panic のみを捕まえる。`GlobalLock` の
/// null ポインタ参照のような未定義動作（segfault 相当）はそもそも Rust の
/// panic ではないため、この hook でも捕まえられない
/// （`win_clip::hglobal_bytes` 側で null チェックして防ぐ必要がある）。
fn install_panic_hook() {
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

fn find_tailscale_ip() -> Option<Ipv4Addr> {
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

fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0xC0) == 0x40
}

// ── Auth (serve) ──────────────────────────────────────────────────────────────

fn check_auth(token: &Option<String>, headers: &HeaderMap) -> bool {
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

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "Unauthorized\n").into_response()
}

// ── HTTP handlers (serve) ─────────────────────────────────────────────────────

async fn handle_health() -> &'static str {
    "OK\n"
}

async fn handle_clip(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }

    let (tx, rx) = oneshot::channel();
    if s.clip_tx.send(ClipRequest::GetClip { reply: tx }).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let Ok(kind) = rx.await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    match kind {
        ClipKind::Image(data) => {
            s.last_clip.lock().unwrap().files.clear();
            clip_resp("image", "image/png", data)
        }
        ClipKind::Files(paths) => {
            let j = serde_json::to_vec(&paths).unwrap();
            s.last_clip.lock().unwrap().files = paths;
            clip_resp("files", "application/json", j)
        }
        ClipKind::VFiles(names) => {
            let j = serde_json::to_vec(&names).unwrap();
            s.last_clip.lock().unwrap().vfiles = names;
            clip_resp("vfiles", "application/json", j)
        }
        ClipKind::Audio(data) => clip_resp("audio", "audio/wav", data),
        ClipKind::Html(html) => clip_resp("html", "text/html; charset=utf-8", html.into_bytes()),
        ClipKind::Url(url) => clip_resp("url", "text/plain; charset=utf-8", url.into_bytes()),
        ClipKind::Rtf(rtf) => clip_resp("rtf", "text/rtf", rtf.into_bytes()),
        ClipKind::Text(text) => clip_resp("text", "text/plain; charset=utf-8", text.into_bytes()),
        ClipKind::Empty => {
            #[cfg(windows)]
            win_clip::show_balloon("クリップボードが空です");
            clip_resp("empty", "text/plain; charset=utf-8", b"".to_vec())
        }
    }
}

fn clip_resp(kind: &str, ct: &str, body: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("X-Clip-Kind", kind)
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from(body))
        .unwrap()
}

async fn handle_clip_post(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }
    let text = match String::from_utf8(body.to_vec()) {
        Ok(t) => t,
        Err(_) => return (StatusCode::BAD_REQUEST, "Body must be UTF-8\n").into_response(),
    };
    let (tx, rx) = oneshot::channel();
    if s.clip_tx
        .send(ClipRequest::SetClip { text, reply: tx })
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match rx.await {
        Ok(Ok(())) => (StatusCode::NO_CONTENT, "").into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}
#[derive(Deserialize)]
struct VFileQuery {
    i: usize,
}
#[derive(Deserialize)]
struct OpenQuery {
    name: String,
}

async fn handle_register(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }

    #[derive(serde::Deserialize)]
    struct Req {
        name: String,
        #[serde(flatten)]
        target: StoredTarget,
    }

    let req: Req = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("JSON parse error: {e}\n")).into_response()
        }
    };

    if let Err(e) = validate_target_name(&req.name) {
        return (StatusCode::BAD_REQUEST, format!("{e}\n")).into_response();
    }

    let entry = req.target;
    let pending_path = s.config_dir.join("pending.toml");
    let registered_path = s.config_dir.join("registered.toml");
    let mut registered = load_target_map_or_warn(&registered_path);

    if s.auto_approve {
        registered.insert(req.name.clone(), entry);
        if let Err(e) = save_target_map(&registered_path, &registered) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("保存エラー: {e}\n"),
            )
                .into_response();
        }
        return (StatusCode::OK, format!("'{}' を登録しました\n", req.name)).into_response();
    }

    // 通常フロー: pending に追加、既承認分は取り消し
    let reapproval = registered.remove(&req.name).is_some();
    let mut pending = load_target_map_or_warn(&pending_path);
    #[cfg(windows)]
    let entry_for_toast = entry.clone();
    pending.insert(req.name.clone(), entry);

    if let Err(e) = save_target_map(&pending_path, &pending)
        .and_then(|_| save_target_map(&registered_path, &registered))
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("保存エラー: {e}\n"),
        )
            .into_response();
    }

    let msg = if reapproval {
        format!(
            "'{}' の設定が変更されました。Windows で clipwire approve {} を実行してください",
            req.name, req.name
        )
    } else {
        format!(
            "'{}' を承認待ちに追加しました。Windows で clipwire approve {} を実行してください",
            req.name, req.name
        )
    };
    #[cfg(windows)]
    win_clip::show_register_toast(
        req.name.clone(),
        entry_for_toast,
        s.config_dir.clone(),
        reapproval,
    );
    #[cfg(not(windows))]
    eprintln!("{msg}");

    (StatusCode::OK, format!("{msg}\n")).into_response()
}

async fn handle_exec(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }

    #[derive(serde::Deserialize)]
    struct Req {
        name: String,
    }

    let req: Req = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("JSON parse error: {e}\n")).into_response()
        }
    };

    if let Err(e) = validate_target_name(&req.name) {
        return (StatusCode::BAD_REQUEST, format!("{e}\n")).into_response();
    }

    let registered = load_target_map_or_warn(&s.config_dir.join("registered.toml"));
    let stored = match registered.get(&req.name) {
        Some(t) => t.clone(),
        None => {
            let pending = load_target_map_or_warn(&s.config_dir.join("pending.toml"));
            if pending.contains_key(&req.name) {
                return (
                    StatusCode::CONFLICT,
                    format!(
                        "'{}' は承認待ちです。Windows で clipwire approve {} を実行してください\n",
                        req.name, req.name
                    ),
                )
                    .into_response();
            }
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    let (dir, payload) = match stored.into_exec() {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")).into_response(),
    };

    match payload {
        ExecPayload::Steps { steps, env } => {
            let mut combined = Vec::new();
            for args in &steps.into_argv() {
                if args.is_empty() {
                    continue;
                }
                let mut cmd = tokio::process::Command::new(&args[0]);
                cmd.args(&args[1..]);
                cmd.envs(&env);
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());
                if let Some(ref d) = dir {
                    cmd.current_dir(d);
                }
                let output = match cmd.output().await {
                    Ok(o) => o,
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("実行エラー: {e}\n"),
                        )
                            .into_response()
                    }
                };
                combined.extend_from_slice(&output.stderr);
                combined.extend_from_slice(&output.stdout);
                if !output.status.success() {
                    return exec_response(combined, output.status.code().unwrap_or(-1));
                }
            }
            exec_response(combined, 0)
        }

        ExecPayload::Script { script } => {
            match tokio::task::spawn_blocking(move || exec_rhai(&script, dir.as_deref())).await {
                Ok(Ok((out, code))) => exec_response(out, code),
                Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")).into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("thread panic: {e}\n"),
                )
                    .into_response(),
            }
        }
    }
}

fn exec_response(body: Vec<u8>, exit_code: i32) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("X-Exit-Code", exit_code.to_string())
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

fn exec_rhai(script: &str, dir: Option<&str>) -> Result<(Vec<u8>, i32)> {
    use std::sync::{Arc, Mutex};
    let out = Arc::new(Mutex::new(Vec::<u8>::new()));
    let dir = dir.map(str::to_string);

    let mut engine = rhai::Engine::new();

    // run(["cmd", "arg", ...]) — 失敗したらスクリプトを停止
    {
        let out = out.clone();
        let dir = dir.clone();
        engine.register_fn(
            "run",
            move |args: rhai::Array| -> Result<(), Box<rhai::EvalAltResult>> {
                let args: Vec<String> = args
                    .iter()
                    .map(|a| {
                        a.clone()
                            .try_cast::<String>()
                            .unwrap_or_else(|| a.to_string())
                    })
                    .collect();
                if args.is_empty() {
                    return Ok(());
                }
                let mut cmd = std::process::Command::new(&args[0]);
                cmd.args(&args[1..]);
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());
                if let Some(ref d) = dir {
                    cmd.current_dir(d);
                }
                let o = cmd.output().map_err(|e| e.to_string())?;
                {
                    let mut g = out.lock().unwrap();
                    g.extend_from_slice(&o.stderr);
                    g.extend_from_slice(&o.stdout);
                }
                if !o.status.success() {
                    return Err(format!("exit code {}", o.status.code().unwrap_or(-1)).into());
                }
                Ok(())
            },
        );
    }

    // run_ok(["cmd", ...]) — 失敗しても続行、成功なら true
    {
        let out = out.clone();
        let dir = dir.clone();
        engine.register_fn("run_ok", move |args: rhai::Array| -> bool {
            let args: Vec<String> = args
                .iter()
                .map(|a| {
                    a.clone()
                        .try_cast::<String>()
                        .unwrap_or_else(|| a.to_string())
                })
                .collect();
            if args.is_empty() {
                return true;
            }
            let mut cmd = std::process::Command::new(&args[0]);
            cmd.args(&args[1..]);
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            if let Some(ref d) = dir {
                cmd.current_dir(d);
            }
            match cmd.output() {
                Ok(o) => {
                    let mut g = out.lock().unwrap();
                    g.extend_from_slice(&o.stderr);
                    g.extend_from_slice(&o.stdout);
                    o.status.success()
                }
                Err(e) => {
                    // run_ok は失敗を無視して続行する設計だが、起動すらできな
                    // かった理由（プログラムが見つからない等）まで無音にする
                    // と原因調査が不可能になるため、出力に残す。
                    let mut g = out.lock().unwrap();
                    g.extend_from_slice(
                        format!("run_ok: {} の起動に失敗しました: {e}\n", args[0]).as_bytes(),
                    );
                    false
                }
            }
        });
    }

    // file_exists(path)
    {
        let dir = dir.clone();
        engine.register_fn("file_exists", move |path: &str| -> bool {
            let p = match &dir {
                Some(d) => std::path::Path::new(d).join(path),
                None => path.into(),
            };
            p.exists()
        });
    }

    // rm(path) — ファイル削除、失敗しても続行
    {
        let dir = dir.clone();
        engine.register_fn("rm", move |path: &str| -> bool {
            let p = match &dir {
                Some(d) => std::path::Path::new(d).join(path),
                None => path.into(),
            };
            std::fs::remove_file(p).is_ok()
        });
    }

    let code = match engine.eval::<()>(script) {
        Ok(_) => 0i32,
        Err(e) => {
            out.lock()
                .unwrap()
                .extend_from_slice(format!("script error: {e}\n").as_bytes());
            1i32
        }
    };
    let bytes = out.lock().unwrap().clone();
    Ok((bytes, code))
}

async fn handle_open(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<OpenQuery>,
) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }
    let url = match q.name.as_str() {
        "chatgpt" => "https://chatgpt.com",
        "claude" => "https://claude.ai",
        "tailscale" => "https://login.tailscale.com/admin",
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown target: {other}\n"),
            )
                .into_response()
        }
    };
    #[cfg(windows)]
    if let Err(e) = std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .spawn()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("ブラウザを開けませんでした: {e}\n"),
        )
            .into_response();
    }
    (StatusCode::OK, format!("{url}\n")).into_response()
}

async fn handle_file(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<FileQuery>,
) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }
    let req_path = match std::path::Path::new(&q.path).canonicalize() {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let allowed = s.last_clip.lock().unwrap().files.iter().any(|f| {
        std::path::Path::new(f)
            .canonicalize()
            .map(|p| p == req_path)
            .unwrap_or(false)
    });
    if !allowed {
        return StatusCode::FORBIDDEN.into_response();
    }
    let (tx, rx) = oneshot::channel();
    if s.clip_tx
        .send(ClipRequest::GetFile {
            path: q.path,
            reply: tx,
        })
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match rx.await {
        Ok(Some(data)) => {
            let mime = mime_for_ext(req_path.extension().and_then(|e| e.to_str()).unwrap_or(""));
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .body(Body::from(data))
                .unwrap()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn handle_vfile(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<VFileQuery>,
) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }
    let (tx, rx) = oneshot::channel();
    if s.clip_tx
        .send(ClipRequest::GetVFile {
            index: q.i,
            reply: tx,
        })
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match rx.await {
        Ok(Some(data)) => {
            let fname = s
                .last_clip
                .lock()
                .unwrap()
                .vfiles
                .get(q.i)
                .cloned()
                .unwrap_or_else(|| format!("file_{}", q.i));
            let mime = mime_for_ext(
                std::path::Path::new(&fname)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or(""),
            );
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header(
                    "Content-Disposition",
                    format!("attachment; filename=\"{fname}\""),
                )
                .body(Body::from(data))
                .unwrap()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn mime_for_ext(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "zip" => "application/zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

fn resolve_serve_token(
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

const CLI_TOKEN_DEPRECATION_WARNING: &str =
    "--token はプロセス一覧に露出するため非推奨です。--token-file を使用してください";

// ── serve entry point ─────────────────────────────────────────────────────────

async fn run_serve(args: ServeArgs) -> Result<()> {
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

    if !args.bind_localhost_only && token.is_none() && !args.allow_no_token {
        bail!(
            "トークンなしでの起動を拒否しました。\n\
             --token-file <path>、CLIPD_TOKEN 環境変数、または --allow-no-token を指定してください。"
        );
    }

    #[cfg(windows)]
    let _mutex = unsafe { win_clip::acquire_mutex() };
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

    let config_dir = clipwire_config_dir();
    warn_invalid_stored_target_names(&config_dir);

    let state = AppState {
        clip_tx,
        token,
        last_clip: Arc::new(Mutex::new(LastClip::default())),
        config_dir,
        auto_approve: args.auto_approve,
    };

    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/", get(handle_clip))
        .route("/clip", get(handle_clip).post(handle_clip_post))
        .route("/file", get(handle_file))
        .route("/vfile", get(handle_vfile))
        .route("/open", get(handle_open))
        .route("/exec", post(handle_exec))
        .route("/register", post(handle_register))
        .with_state(state.clone());

    let localhost = SocketAddr::from(([127, 0, 0, 1], args.port));

    if args.bind_localhost_only {
        serve_forever(localhost, app, "localhost").await;
    } else {
        match find_tailscale_ip() {
            Some(ts_ip) => {
                let ts_addr = SocketAddr::from((ts_ip, args.port));
                let app2 = app.clone();
                tokio::spawn(serve_forever(localhost, app2, "localhost"));
                serve_forever(ts_addr, app, "tailscale").await;
            }
            None => {
                warn!("Tailscale IP not found; falling back to localhost-only");
                serve_forever(localhost, app, "localhost").await;
            }
        }
    }
    Ok(())
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
async fn serve_forever(addr: SocketAddr, app: Router, label: &str) {
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                info!("Listening on http://{addr} ({label})");
                if let Err(e) = axum::serve(listener, app.clone()).await {
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
    use proptest::prelude::*;
    use quick_xml::{events::Event, Reader};
    use std::collections::HashMap;
    use tempfile::tempdir;
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
