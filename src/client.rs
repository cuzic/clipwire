use super::*;

// ── Client config ─────────────────────────────────────────────────────────────

pub(crate) struct ClientConfig {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) token: Option<String>,
}

impl ClientConfig {
    pub(crate) fn from_env() -> Result<Self> {
        let host = std::env::var("CLIPD_HOST")
            .context("CLIPD_HOST が設定されていません (例: export CLIPD_HOST=my-windows)")?;
        let port = std::env::var("CLIPD_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9999u16);
        let token = std::env::var("CLIPD_TOKEN").ok();
        Ok(Self { host, port, token })
    }

    pub(crate) fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    pub(crate) fn set_auth(&self, req: ureq::Request) -> ureq::Request {
        if let Some(token) = &self.token {
            req.set("Authorization", &format!("Bearer {}", token))
        } else {
            req
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ServerCapabilities {
    #[serde(default = "unknown_server_version")]
    pub(crate) version: String,
    #[serde(default = "legacy_protocol_version")]
    pub(crate) proto: u32,
    #[serde(default)]
    pub(crate) features: Vec<String>,
}

fn unknown_server_version() -> String {
    "unknown".into()
}

const fn legacy_protocol_version() -> u32 {
    1
}

impl Default for ServerCapabilities {
    fn default() -> Self {
        Self {
            version: unknown_server_version(),
            proto: legacy_protocol_version(),
            features: Vec::new(),
        }
    }
}

fn discover_capabilities(cfg: &ClientConfig) -> ServerCapabilities {
    let url = format!("{}/health", cfg.base_url());
    let Ok(response) = ureq::get(&url)
        .set("Accept", "application/json")
        .timeout(Duration::from_secs(10))
        .call()
    else {
        return ServerCapabilities::default();
    };
    if !response
        .header("Content-Type")
        .is_some_and(|value| value.split(';').next() == Some("application/json"))
    {
        return ServerCapabilities::default();
    }
    response
        .into_string()
        .ok()
        .and_then(|body| serde_json::from_str::<ServerCapabilities>(&body).ok())
        .unwrap_or_default()
}

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TargetState {
    Ok,
    Changed,
    Pending,
    Unregistered,
    RemoteOnly,
}

impl TargetState {
    const ALL: [Self; 5] = [
        Self::Ok,
        Self::Changed,
        Self::Pending,
        Self::Unregistered,
        Self::RemoteOnly,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Changed => "changed",
            Self::Pending => "pending",
            Self::Unregistered => "unregistered",
            Self::RemoteOnly => "remote-only",
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct CheckTarget {
    #[serde(rename = "status")]
    state: TargetState,
    hash: String,
}

#[derive(Debug, serde::Deserialize)]
struct CheckResponse {
    targets: std::collections::BTreeMap<String, CheckTarget>,
}

#[derive(Debug, serde::Serialize)]
struct ListEntry {
    name: String,
    state: TargetState,
    hash: String,
}

fn list_entries(response: CheckResponse) -> Vec<ListEntry> {
    response
        .targets
        .into_iter()
        .map(|(name, target)| ListEntry {
            name,
            state: target.state,
            hash: target.hash,
        })
        .collect()
}

fn state_counts(entries: &[ListEntry]) -> [(TargetState, usize); 5] {
    TargetState::ALL.map(|state| {
        let count = entries.iter().filter(|entry| entry.state == state).count();
        (state, count)
    })
}

fn format_target_list(entries: &[ListEntry]) -> String {
    let mut output = String::from("NAME\tSTATE\tHASH\n");
    for entry in entries {
        output.push_str(&format!(
            "{}\t{}\t{}\n",
            entry.name,
            entry.state.label(),
            entry.hash
        ));
    }
    output.push_str("集計:");
    for (state, count) in state_counts(entries) {
        output.push_str(&format!(" {}={count}", state.label()));
    }
    output.push('\n');
    output
}

fn format_target_json(entries: &[ListEntry]) -> Result<String> {
    Ok(serde_json::to_string_pretty(entries)?)
}

fn is_unsupported_status(status: u16) -> bool {
    status == 404
}

fn fetch_target_list(cfg: &ClientConfig) -> Result<Vec<ListEntry>> {
    let targets = load_local_targets()?;
    check_targets(cfg, &targets)
}

fn check_targets(
    cfg: &ClientConfig,
    targets: &std::collections::BTreeMap<String, StoredTarget>,
) -> Result<Vec<ListEntry>> {
    let body = serde_json::json!({ "targets": targets }).to_string();
    let url = format!("{}/targets/check", cfg.base_url());
    let response = match cfg
        .set_auth(
            ureq::post(&url)
                .set("Content-Type", "application/json")
                .timeout(Duration::from_secs(30)),
        )
        .send_string(&body)
    {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) if is_unsupported_status(code) => {
            bail!("サーバが未対応: /targets/check")
        }
        Err(ureq::Error::Status(401, _)) => bail!("Unauthorized (CLIPD_TOKEN を確認)"),
        Err(ureq::Error::Status(code, response)) => bail!(
            "HTTP {}: {}",
            code,
            response.into_string().unwrap_or_default().trim()
        ),
        Err(error) => bail!("{} に接続できません: {}", cfg.base_url(), error),
    };
    let response: CheckResponse = serde_json::from_reader(response.into_reader())?;
    Ok(list_entries(response))
}

pub(crate) fn cmd_list(cfg: &ClientConfig, args: &ListArgs) -> Result<()> {
    let entries = fetch_target_list(cfg)?;
    if args.json {
        println!("{}", format_target_json(&entries)?);
    } else {
        print!("{}", format_target_list(&entries));
    }
    Ok(())
}

fn running_jobs(cfg: &ClientConfig) -> Result<Vec<crate::jobs::JobMeta>> {
    let url = format!("{}/jobs?state=running", cfg.base_url());
    let response = match cfg
        .set_auth(ureq::get(&url).timeout(Duration::from_secs(30)))
        .call()
    {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) if is_unsupported_status(code) => {
            bail!("ジョブ API 未対応")
        }
        Err(ureq::Error::Status(401, _)) => bail!("Unauthorized (CLIPD_TOKEN を確認)"),
        Err(ureq::Error::Status(code, response)) => bail!(
            "HTTP {}: {}",
            code,
            response.into_string().unwrap_or_default().trim()
        ),
        Err(error) => bail!("{} に接続できません: {}", cfg.base_url(), error),
    };
    Ok(serde_json::from_reader(response.into_reader())?)
}

fn format_server(capabilities: &ServerCapabilities) -> String {
    let features = if capabilities.features.is_empty() {
        "なし".into()
    } else {
        capabilities.features.join(",")
    };
    format!(
        "サーバー: version={} proto={} features={}\n",
        capabilities.version, capabilities.proto, features
    )
}

fn format_running_jobs(jobs: &[crate::jobs::JobMeta]) -> String {
    if jobs.is_empty() {
        return "実行中ジョブ: なし\n".into();
    }
    let mut output = String::from("実行中ジョブ:\nID\tTARGET\n");
    for job in jobs {
        output.push_str(&format!("{}\t{}\n", job.id, job.target));
    }
    output
}

pub(crate) fn cmd_status(cfg: &ClientConfig) -> Result<()> {
    let capabilities = discover_capabilities(cfg);
    let entries = fetch_target_list(cfg)?;
    let jobs = running_jobs(cfg)?;
    print!("{}", format_server(&capabilities));
    print!("{}", format_target_list(&entries));
    print!("{}", format_running_jobs(&jobs));
    Ok(())
}

#[cfg(test)]
mod list_status_tests {
    use super::*;

    fn all_states() -> Vec<ListEntry> {
        [
            ("changed-target", TargetState::Changed, "sha256:changed"),
            ("ok-target", TargetState::Ok, "sha256:ok"),
            ("pending-target", TargetState::Pending, "sha256:pending"),
            ("remote-target", TargetState::RemoteOnly, "sha256:remote"),
            (
                "unregistered-target",
                TargetState::Unregistered,
                "sha256:unregistered",
            ),
        ]
        .into_iter()
        .map(|(name, state, hash)| ListEntry {
            name: name.into(),
            state,
            hash: hash.into(),
        })
        .collect()
    }

    #[test]
    fn ac_t7_2_1_all_states_and_counts_are_table_driven() {
        let rendered = format_target_list(&all_states());
        for state in TargetState::ALL {
            assert!(rendered.contains(&format!("{}=1", state.label())));
            assert!(rendered.contains(state.label()));
        }
    }

    #[test]
    fn ac_t7_2_2_json_matches_golden_schema() {
        let rendered = format!("{}\n", format_target_json(&all_states()).unwrap());
        assert_eq!(rendered, include_str!("../tests/fixtures/list.json"));
    }

    #[test]
    fn ac_t7_2_3_unsupported_detection_is_pure() {
        assert!(is_unsupported_status(404));
        assert!(!is_unsupported_status(401));
        assert!(!is_unsupported_status(500));
    }

    #[test]
    fn ac_t7_3_1_running_job_empty_display() {
        assert_eq!(format_running_jobs(&[]), "実行中ジョブ: なし\n");
        let job = crate::jobs::JobMeta {
            id: "01TEST".into(),
            target: "build".into(),
            def_hash: "sha256:test".into(),
            started_at: 1,
            ended_at: None,
            state: crate::jobs::JobStatus::Running,
            exit_code: None,
            requester: None,
            child: None,
            detached: false,
        };
        assert_eq!(
            format_running_jobs(&[job]),
            "実行中ジョブ:\nID\tTARGET\n01TEST\tbuild\n"
        );
    }

    #[test]
    fn ac_t7_3_2_health_defaults_missing_proto_to_one() {
        let capabilities: ServerCapabilities =
            serde_json::from_str(r#"{"version":"old","features":[]}"#).unwrap();
        assert_eq!(capabilities.proto, 1);
        assert_eq!(
            format_server(&capabilities),
            "サーバー: version=old proto=1 features=なし\n"
        );
    }

    fn one_response_server(
        status: &'static str,
        body: &'static str,
    ) -> Option<(ClientConfig, std::thread::JoinHandle<()>)> {
        use std::io::{Read, Write};
        let listener = match std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("bind mock server: {error}"),
        };
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request).unwrap();
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        Some((
            ClientConfig {
                host: std::net::Ipv4Addr::LOCALHOST.to_string(),
                port,
                token: None,
            },
            server,
        ))
    }

    #[test]
    fn ac_t7_2_1_mock_server_returns_all_five_states() {
        let body = r#"{"targets":{"a":{"status":"ok","hash":"h1"},"b":{"status":"changed","hash":"h2"},"c":{"status":"pending","hash":"h3"},"d":{"status":"unregistered","hash":"h4"},"e":{"status":"remote-only","hash":"h5"}}}"#;
        let Some((cfg, server)) = one_response_server("200 OK", body) else {
            return;
        };
        let entries = check_targets(&cfg, &std::collections::BTreeMap::new()).unwrap();
        assert_eq!(state_counts(&entries).map(|(_, count)| count), [1; 5]);
        server.join().unwrap();
    }

    #[test]
    fn ac_t7_2_3_mock_server_404_is_an_error() {
        let Some((cfg, server)) = one_response_server("404 Not Found", "") else {
            return;
        };
        let error = check_targets(&cfg, &std::collections::BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("サーバが未対応"));
        server.join().unwrap();
    }

    #[test]
    fn ac_t7_3_1_mock_jobs_supports_running_and_empty() {
        for (body, expected) in [
            ("[]", "実行中ジョブ: なし\n"),
            (
                r#"[{"id":"J1","target":"build","def_hash":"h","started_at":1,"state":"running","detached":false}]"#,
                "実行中ジョブ:\nID\tTARGET\nJ1\tbuild\n",
            ),
        ] {
            let Some((cfg, server)) = one_response_server("200 OK", body) else {
                return;
            };
            let jobs = running_jobs(&cfg).unwrap();
            assert_eq!(format_running_jobs(&jobs), expected);
            server.join().unwrap();
        }
    }
}

pub(crate) fn require_features(capabilities: &ServerCapabilities, required: &[&str]) -> Result<()> {
    for feature in required {
        if !capabilities.features.iter().any(|value| value == feature) {
            bail!("サーバーが必要な機能 '{feature}' に対応していません");
        }
    }
    Ok(())
}

pub(crate) fn register_body(
    capabilities: &ServerCapabilities,
    name: &str,
    target: &StoredTarget,
) -> serde_json::Value {
    if capabilities.proto >= 2 {
        serde_json::json!({ "name": name, "target": target })
    } else {
        let mut body =
            serde_json::to_value(target).expect("StoredTarget serialization cannot fail");
        body.as_object_mut()
            .expect("StoredTarget serializes as an object")
            .insert("name".into(), name.into());
        body
    }
}

// ── Client: get ───────────────────────────────────────────────────────────────

pub(crate) fn cmd_get(cfg: &ClientConfig, args: &GetArgs) -> Result<()> {
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

pub(crate) fn cmd_put(cfg: &ClientConfig) -> Result<()> {
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

const EXEC_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const EXEC_STREAM_READ_TIMEOUT: Duration = Duration::from_secs(120);
const EXEC_BUFFERED_READ_TIMEOUT: Duration = Duration::from_secs(31 * 60);

#[derive(Clone, Copy)]
struct ExecClientTimeouts {
    connect: Duration,
    stream_read: Duration,
    buffered_read: Duration,
}

impl Default for ExecClientTimeouts {
    fn default() -> Self {
        Self {
            connect: EXEC_CONNECT_TIMEOUT,
            stream_read: EXEC_STREAM_READ_TIMEOUT,
            buffered_read: EXEC_BUFFERED_READ_TIMEOUT,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ExecResponseMode {
    Stream,
    Buffered,
}

fn exec_response_mode(content_type: Option<&str>, requested_stream: bool) -> ExecResponseMode {
    if requested_stream
        && content_type.is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/x-ndjson"))
        })
    {
        ExecResponseMode::Stream
    } else {
        ExecResponseMode::Buffered
    }
}

fn should_request_exec_stream(
    capabilities: &ServerCapabilities,
    no_stream: bool,
    detach: bool,
) -> bool {
    !no_stream
        && !detach
        && capabilities
            .features
            .iter()
            .any(|feature| feature == "stream")
}

#[derive(Debug, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "t")]
enum ExecStreamEvent {
    #[serde(rename = "out")]
    Out { d: String },
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "exit")]
    Exit { code: i32 },
    #[serde(rename = "err")]
    Err { msg: String },
}

fn parse_exec_event(line: &str) -> Result<ExecStreamEvent> {
    serde_json::from_str(line).context("不正な NDJSON イベントです")
}

fn disconnected_job_message(job_id: Option<&str>) -> String {
    match job_id {
        Some(id) => format!(
            "ストリームが exit イベントなしで切断されました (ジョブ ID: {id})。clipwire logs {id} でログを確認してください"
        ),
        None => "ストリームが exit イベントなしで切断されました (ジョブ ID を取得できませんでした)".into(),
    }
}

fn stream_exit_result(exit_code: Option<i32>, job_id: Option<&str>) -> Result<()> {
    match exit_code {
        Some(0) => Ok(()),
        Some(code) => bail!("exit code {code}"),
        None => bail!("{}", disconnected_job_message(job_id)),
    }
}

const MAX_COPY_BYTES: usize = 1024 * 1024;
const TRUNCATED_PREFIX: &[u8] = b"[truncated]";

fn should_copy_exec_output(copy: bool, copy_on_fail: bool, exit_code: i32) -> bool {
    copy || (copy_on_fail && exit_code != 0)
}

fn strip_ansi(input: &[u8]) -> Vec<u8> {
    #[derive(Clone, Copy)]
    enum State {
        Text,
        Escape,
        Csi,
        Osc,
        OscEscape,
    }
    let mut state = State::Text;
    let mut output = Vec::with_capacity(input.len());
    for &byte in input {
        state = match state {
            State::Text if byte == 0x1b => State::Escape,
            State::Text => {
                output.push(byte);
                State::Text
            }
            State::Escape if byte == b'[' => State::Csi,
            State::Escape if byte == b']' => State::Osc,
            State::Escape => State::Text,
            State::Csi if (0x40..=0x7e).contains(&byte) => State::Text,
            State::Csi => State::Csi,
            State::Osc if byte == 0x07 => State::Text,
            State::Osc if byte == 0x1b => State::OscEscape,
            State::Osc => State::Osc,
            State::OscEscape if byte == b'\\' => State::Text,
            State::OscEscape if byte == 0x1b => State::OscEscape,
            State::OscEscape => State::Osc,
        };
    }
    output
}

fn truncate_copy_output(input: &[u8]) -> Vec<u8> {
    if input.len() <= MAX_COPY_BYTES {
        return input.to_vec();
    }
    let keep = MAX_COPY_BYTES - TRUNCATED_PREFIX.len();
    let mut start = input.len() - keep;
    while start < input.len() && (input[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    let mut output = Vec::with_capacity(MAX_COPY_BYTES);
    output.extend_from_slice(TRUNCATED_PREFIX);
    output.extend_from_slice(&input[start..]);
    output
}

fn prepare_copy_output(input: &[u8], raw: bool) -> Vec<u8> {
    let utf8 = String::from_utf8_lossy(input);
    let cleaned = if raw {
        utf8.as_bytes().to_vec()
    } else {
        strip_ansi(utf8.as_bytes())
    };
    truncate_copy_output(&cleaned)
}

fn send_exec_output_to_clipboard(cfg: &ClientConfig, output: &[u8]) -> Result<()> {
    let url = format!("{}/clip", cfg.base_url());
    let req = cfg.set_auth(
        ureq::post(&url)
            .set("Content-Type", "text/plain; charset=utf-8")
            .timeout(Duration::from_secs(30)),
    );
    match req.send_bytes(output) {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, response)) => bail!(
            "HTTP {}: {}",
            code,
            response.into_string().unwrap_or_default().trim()
        ),
        Err(error) => bail!("{} への送信に失敗: {}", cfg.base_url(), error),
    }
}

pub(crate) fn cmd_exec(cfg: &ClientConfig, args: &ExecArgs) -> Result<()> {
    cmd_exec_with_io(
        cfg,
        args,
        ExecClientTimeouts::default(),
        &mut io::stdout(),
        &mut io::stderr(),
    )
}

fn cmd_exec_with_io(
    cfg: &ClientConfig,
    args: &ExecArgs,
    timeouts: ExecClientTimeouts,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<()> {
    validate_target_name(&args.target)?;
    let capabilities = discover_capabilities(cfg);
    let mut required = Vec::new();
    if args.timeout.is_some() {
        required.push("timeout");
    }
    if args.detach {
        required.push("jobs");
    }
    require_features(&capabilities, &required)?;
    let requested_stream = should_request_exec_stream(&capabilities, args.no_stream, args.detach);
    let body =
        serde_json::json!({ "name": args.target, "timeout": args.timeout, "detach": args.detach })
            .to_string();
    let url = format!("{}/exec", cfg.base_url());
    let read_timeout = if requested_stream {
        timeouts.stream_read
    } else {
        timeouts.buffered_read
    };
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeouts.connect)
        .timeout_read(read_timeout)
        .build();
    let mut req = agent.post(&url).set("Content-Type", "application/json");
    if requested_stream {
        req = req.set("Accept", "application/x-ndjson");
    }
    let req = cfg.set_auth(req);
    let resp = match req.send_string(&body) {
        Ok(r) => r,
        Err(ureq::Error::Status(401, _)) => bail!("Unauthorized (CLIPD_TOKEN を確認)"),
        Err(ureq::Error::Status(404, _)) => bail!(
            "'{}' は Windows 側に登録されていません。先に clipwire register を実行してください",
            args.target
        ),
        Err(ureq::Error::Status(409, r)) => bail!("{}", r.into_string().unwrap_or_default().trim()),
        Err(ureq::Error::Status(503, r)) => bail!("{}", r.into_string().unwrap_or_default().trim()),
        Err(ureq::Error::Status(code, r)) => {
            bail!(
                "HTTP {}: {}",
                code,
                r.into_string().unwrap_or_default().trim()
            )
        }
        Err(e) => bail!("{} に接続できません: {}", cfg.base_url(), e),
    };
    if args.detach {
        let value: serde_json::Value = serde_json::from_reader(resp.into_reader())?;
        writeln!(
            stdout,
            "{}",
            value["id"]
                .as_str()
                .context("応答にジョブ ID がありません")?
        )?;
        return Ok(());
    }
    let mut captured = Vec::new();
    let exit_code = if exec_response_mode(resp.header("Content-Type"), requested_stream)
        == ExecResponseMode::Buffered
    {
        let exit_code = resp
            .header("X-Exit-Code")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let mut reader = resp.into_reader();
        reader.read_to_end(&mut captured)?;
        stdout.write_all(&captured)?;
        exit_code
    } else {
        let job_id = resp.header("X-Job-Id").map(str::to_owned);
        let mut stream_exit_code = None;
        let reader = io::BufReader::new(resp.into_reader());
        for line in std::io::BufRead::lines(reader) {
            let Ok(line) = line else {
                break;
            };
            match parse_exec_event(&line)? {
                ExecStreamEvent::Out { d } => {
                    captured.extend_from_slice(d.as_bytes());
                    stdout.write_all(d.as_bytes())?;
                    stdout.flush()?;
                }
                ExecStreamEvent::Ping => {}
                ExecStreamEvent::Exit { code } => {
                    stream_exit_code = Some(code);
                    break;
                }
                ExecStreamEvent::Err { msg } => writeln!(stderr, "server error: {msg}")?,
            }
        }
        if stream_exit_code.is_none() {
            writeln!(stderr, "{}", disconnected_job_message(job_id.as_deref()))?;
            return stream_exit_result(stream_exit_code, job_id.as_deref());
        }
        stream_exit_code.unwrap_or_default()
    };
    if should_copy_exec_output(args.copy, args.copy_on_fail, exit_code) {
        let copy = prepare_copy_output(&captured, args.raw);
        if let Err(error) = send_exec_output_to_clipboard(cfg, &copy) {
            writeln!(
                stderr,
                "warning: 実行結果をクリップボードへ送信できませんでした: {error}"
            )?;
        }
    }
    stream_exit_result(Some(exit_code), None)
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod exec_tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

    fn test_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn read_request(stream: &mut TcpStream) -> String {
        read_request_with_body(stream).0
    }

    fn read_request_with_body(stream: &mut TcpStream) -> (String, Vec<u8>) {
        let mut bytes = Vec::new();
        let mut byte = [0_u8; 1];
        while !bytes.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
        }
        let headers = String::from_utf8(bytes).unwrap();
        let length = headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        (headers, body)
    }

    fn mock_server<F>(exec: F) -> Option<(ClientConfig, thread::JoinHandle<()>)>
    where
        F: FnOnce(TcpStream, String) + Send + 'static,
    {
        let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("mock server bind failed: {error}"),
        };
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut health, _) = listener.accept().unwrap();
            let _ = read_request(&mut health);
            health
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 50\r\nConnection: close\r\n\r\n{\"proto\":2,\"features\":[\"timeout\",\"jobs\",\"stream\"]}",
                )
                .unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            exec(stream, request);
        });
        Some((
            ClientConfig {
                host: Ipv4Addr::LOCALHOST.to_string(),
                port,
                token: None,
            },
            handle,
        ))
    }

    fn args(no_stream: bool) -> ExecArgs {
        ExecArgs {
            target: "test".into(),
            timeout: None,
            no_stream,
            detach: false,
            copy: false,
            copy_on_fail: false,
            raw: false,
        }
    }

    fn short_timeouts() -> ExecClientTimeouts {
        ExecClientTimeouts {
            connect: Duration::from_secs(1),
            stream_read: Duration::from_millis(500),
            buffered_read: Duration::from_secs(3),
        }
    }

    type CopyServer = (
        ClientConfig,
        Arc<Mutex<Option<Vec<u8>>>>,
        thread::JoinHandle<()>,
    );

    fn copy_server(
        output: Vec<u8>,
        exit_code: i32,
        clip_status: u16,
        expect_clip: bool,
    ) -> Option<CopyServer> {
        let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("mock server bind failed: {error}"),
        };
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(None));
        let received_by_server = received.clone();
        let handle = thread::spawn(move || {
            let (mut health, _) = listener.accept().unwrap();
            let _ = read_request(&mut health);
            health.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 50\r\nConnection: close\r\n\r\n{\"proto\":2,\"features\":[\"timeout\",\"jobs\",\"stream\"]}").unwrap();

            let (mut exec, _) = listener.accept().unwrap();
            let _ = read_request(&mut exec);
            write!(exec, "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Exit-Code: {exit_code}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", output.len()).unwrap();
            exec.write_all(&output).unwrap();
            exec.flush().unwrap();

            if expect_clip {
                let (mut clip, _) = listener.accept().unwrap();
                let (headers, body) = read_request_with_body(&mut clip);
                assert!(headers.starts_with("POST /clip HTTP/1.1"));
                *received_by_server.lock().unwrap() = Some(body);
                write!(
                    clip,
                    "HTTP/1.1 {clip_status} Mock\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            } else {
                listener.set_nonblocking(true).unwrap();
                for _ in 0..20 {
                    match listener.accept() {
                        Ok(_) => panic!("unexpected POST /clip"),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("mock server accept failed: {error}"),
                    }
                }
            }
        });
        Some((
            ClientConfig {
                host: Ipv4Addr::LOCALHOST.to_string(),
                port,
                token: None,
            },
            received,
            handle,
        ))
    }

    #[test]
    fn ac_t6_4_1_stream_has_no_overall_timeout() {
        let _guard = test_lock();
        let Some((cfg, server)) = mock_server(|mut stream, request| {
            assert!(request.contains("Accept: application/x-ndjson"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nX-Job-Id: long-job\r\nConnection: close\r\n\r\n")
                .unwrap();
            for _ in 0..25 {
                thread::sleep(Duration::from_millis(25));
                stream.write_all(b"{\"t\":\"ping\"}\n").unwrap();
                stream.flush().unwrap();
            }
            stream.write_all(b"{\"t\":\"exit\",\"code\":0}\n").unwrap();
        }) else {
            return;
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        cmd_exec_with_io(&cfg, &args(false), short_timeouts(), &mut out, &mut err).unwrap();
        server.join().unwrap();
        assert!(err.is_empty());
    }

    #[test]
    fn ac_t6_4_2_no_stream_uses_buffered_timeout() {
        let _guard = test_lock();
        let Some((cfg, server)) = mock_server(|mut stream, request| {
            assert!(!request.contains("Accept: application/x-ndjson"));
            thread::sleep(Duration::from_millis(700));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Exit-Code: 0\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone").unwrap();
        }) else {
            return;
        };
        let mut out = Vec::new();
        cmd_exec_with_io(
            &cfg,
            &args(true),
            short_timeouts(),
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(out, b"done");
    }

    #[test]
    fn ac_t6_4_3_text_plain_response_falls_back_to_buffered() {
        let _guard = test_lock();
        let Some((cfg, server)) = mock_server(|mut stream, request| {
            assert!(request.contains("Accept: application/x-ndjson"));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nX-Exit-Code: 0\r\nContent-Length: 6\r\nConnection: close\r\n\r\nlegacy").unwrap();
        }) else {
            return;
        };
        let mut out = Vec::new();
        cmd_exec_with_io(
            &cfg,
            &args(false),
            short_timeouts(),
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(out, b"legacy");
        assert!(!should_request_exec_stream(
            &ServerCapabilities::default(),
            false,
            false
        ));
    }

    #[test]
    fn ac_t6_4_4_missing_exit_reports_job_id() {
        let _guard = test_lock();
        let Some((cfg, server)) = mock_server(|mut stream, _| {
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nX-Job-Id: recover-me\r\nConnection: close\r\n\r\n{\"t\":\"out\",\"d\":\"partial\"}\n").unwrap();
        }) else {
            return;
        };
        let mut err = Vec::new();
        let result = cmd_exec_with_io(
            &cfg,
            &args(false),
            short_timeouts(),
            &mut Vec::new(),
            &mut err,
        );
        server.join().unwrap();
        assert!(result.is_err());
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("recover-me"));
        assert!(err.contains("clipwire logs recover-me"));
    }

    #[test]
    fn ac_t6_4_5_buffered_output_exceeds_ten_mib() {
        let _guard = test_lock();
        const SIZE: usize = 10 * 1024 * 1024 + 1;
        let Some((cfg, server)) = mock_server(|mut stream, _| {
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Exit-Code: 0\r\nContent-Length: {SIZE}\r\nConnection: close\r\n\r\n").unwrap();
            stream.write_all(&vec![b'x'; SIZE]).unwrap();
        }) else {
            return;
        };
        let mut out = Vec::new();
        cmd_exec_with_io(
            &cfg,
            &args(true),
            short_timeouts(),
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(out.len(), SIZE);
    }

    #[test]
    fn ac_t7_1_1_output_is_posted_to_clip() {
        let _guard = test_lock();
        let Some((cfg, received, server)) = copy_server(b"build output\n".to_vec(), 0, 204, true)
        else {
            return;
        };
        let mut copy_args = args(true);
        copy_args.copy = true;
        cmd_exec_with_io(
            &cfg,
            &copy_args,
            short_timeouts(),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(*received.lock().unwrap(), Some(b"build output\n".to_vec()));
    }

    #[test]
    fn ac_t7_1_2_copy_keeps_tail_with_one_mib_limit() {
        let _guard = test_lock();
        let mut output = vec![b'a'; 2 * 1024 * 1024];
        output.extend_from_slice(b"tail");
        let Some((cfg, received, server)) = copy_server(output, 0, 204, true) else {
            return;
        };
        let mut copy_args = args(true);
        copy_args.copy = true;
        cmd_exec_with_io(
            &cfg,
            &copy_args,
            short_timeouts(),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap();
        server.join().unwrap();
        let body = received.lock().unwrap().clone().unwrap();
        assert!(body.len() <= MAX_COPY_BYTES);
        assert!(body.starts_with(TRUNCATED_PREFIX));
        assert!(body.ends_with(b"tail"));
    }

    #[test]
    fn ac_t7_1_3_ansi_is_removed_unless_raw() {
        let _guard = test_lock();
        let Some((cfg, received, server)) =
            copy_server(b"\x1b[31mred\x1b[0m".to_vec(), 0, 204, true)
        else {
            return;
        };
        let mut copy_args = args(true);
        copy_args.copy = true;
        cmd_exec_with_io(
            &cfg,
            &copy_args,
            short_timeouts(),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(*received.lock().unwrap(), Some(b"red".to_vec()));

        let Some((cfg, received, server)) =
            copy_server(b"\x1b[31mred\x1b[0m".to_vec(), 0, 204, true)
        else {
            return;
        };
        let mut raw_args = args(true);
        raw_args.copy = true;
        raw_args.raw = true;
        cmd_exec_with_io(
            &cfg,
            &raw_args,
            short_timeouts(),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(
            *received.lock().unwrap(),
            Some(b"\x1b[31mred\x1b[0m".to_vec())
        );
        assert_eq!(prepare_copy_output(b"\x1b]0;title\x07text", false), b"text");
    }

    #[test]
    fn ac_t7_1_4_copy_on_fail_uses_exit_code() {
        let _guard = test_lock();
        let Some((cfg, _, server)) = copy_server(b"ok".to_vec(), 0, 204, false) else {
            return;
        };
        let mut success_args = args(true);
        success_args.copy_on_fail = true;
        cmd_exec_with_io(
            &cfg,
            &success_args,
            short_timeouts(),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap();
        server.join().unwrap();

        let Some((cfg, received, server)) = copy_server(b"failed".to_vec(), 7, 204, true) else {
            return;
        };
        let mut failure_args = args(true);
        failure_args.copy_on_fail = true;
        let error = cmd_exec_with_io(
            &cfg,
            &failure_args,
            short_timeouts(),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
        server.join().unwrap();
        assert!(error.to_string().contains("exit code 7"));
        assert_eq!(*received.lock().unwrap(), Some(b"failed".to_vec()));
    }

    #[test]
    fn ac_t7_1_5_clip_failure_preserves_job_failure() {
        let _guard = test_lock();
        let Some((cfg, _, server)) = copy_server(b"failed".to_vec(), 9, 500, true) else {
            return;
        };
        let mut args = args(true);
        args.copy = true;
        let mut stderr = Vec::new();
        let error = cmd_exec_with_io(&cfg, &args, short_timeouts(), &mut Vec::new(), &mut stderr)
            .unwrap_err();
        server.join().unwrap();
        assert!(error.to_string().contains("exit code 9"));
        assert!(String::from_utf8(stderr).unwrap().contains("warning:"));
    }

    #[test]
    fn ac_t7_7_1_open_client_uses_json_post() {
        let _guard = test_lock();
        let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("mock server bind failed: {error}"),
        };
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (headers, body) = read_request_with_body(&mut stream);
            assert!(headers.starts_with("POST /open HTTP/1.1"));
            assert!(headers.contains("Content-Type: application/json"));
            assert_eq!(body, br#"{"name":"chatgpt"}"#);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        let cfg = ClientConfig {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port,
            token: None,
        };
        cmd_open(
            &cfg,
            &OpenArgs {
                target: OpenTarget::Chatgpt,
            },
        )
        .unwrap();
        server.join().unwrap();
    }

    #[test]
    fn open_client_falls_back_to_get_for_old_server() {
        let _guard = test_lock();
        let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("mock server bind failed: {error}"),
        };
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut post, _) = listener.accept().unwrap();
            assert!(read_request(&mut post).starts_with("POST /open HTTP/1.1"));
            post.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            let (mut get, _) = listener.accept().unwrap();
            assert!(read_request(&mut get).starts_with("GET /open?name=claude HTTP/1.1"));
            get.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        let cfg = ClientConfig {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port,
            token: None,
        };
        cmd_open(
            &cfg,
            &OpenArgs {
                target: OpenTarget::Claude,
            },
        )
        .unwrap();
        server.join().unwrap();
    }
}

fn jobs_request(cfg: &ClientConfig, path: &str) -> Result<ureq::Response> {
    require_features(&discover_capabilities(cfg), &["jobs"])?;
    let url = format!("{}{}", cfg.base_url(), path);
    match cfg
        .set_auth(ureq::get(&url).timeout(Duration::from_secs(30)))
        .call()
    {
        Ok(response) => Ok(response),
        Err(ureq::Error::Status(401, _)) => bail!("Unauthorized (CLIPD_TOKEN を確認)"),
        Err(ureq::Error::Status(404, _)) => bail!("ジョブが見つかりません"),
        Err(ureq::Error::Status(code, response)) => bail!(
            "HTTP {}: {}",
            code,
            response.into_string().unwrap_or_default().trim()
        ),
        Err(error) => bail!("{} に接続できません: {}", cfg.base_url(), error),
    }
}

pub(crate) fn cmd_jobs(cfg: &ClientConfig) -> Result<()> {
    let response = jobs_request(cfg, "/jobs")?;
    let jobs: Vec<crate::jobs::JobMeta> = serde_json::from_reader(response.into_reader())?;
    for job in jobs {
        println!("{}\t{:?}\t{}", job.id, job.state, job.target);
    }
    Ok(())
}

pub(crate) fn cmd_logs(cfg: &ClientConfig, args: &JobIdArgs) -> Result<()> {
    let response = jobs_request(cfg, &format!("/jobs/{}/log", args.id))?;
    io::copy(&mut response.into_reader(), &mut io::stdout())?;
    Ok(())
}

pub(crate) fn cmd_kill(cfg: &ClientConfig, args: &JobIdArgs) -> Result<()> {
    require_features(&discover_capabilities(cfg), &["jobs"])?;
    let url = format!("{}/jobs/{}/kill", cfg.base_url(), args.id);
    match cfg
        .set_auth(
            ureq::post(&url)
                .set("Content-Type", "application/json")
                .timeout(Duration::from_secs(30)),
        )
        .send_string("{}")
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(401, _)) => bail!("Unauthorized (CLIPD_TOKEN を確認)"),
        Err(ureq::Error::Status(404, _)) => bail!("ジョブが見つかりません"),
        Err(ureq::Error::Status(409, response)) => {
            bail!("{}", response.into_string().unwrap_or_default().trim())
        }
        Err(ureq::Error::Status(code, response)) => bail!(
            "HTTP {}: {}",
            code,
            response.into_string().unwrap_or_default().trim()
        ),
        Err(error) => bail!("{} に接続できません: {}", cfg.base_url(), error),
    }
}

// ── Client: register ──────────────────────────────────────────────────────────

pub(crate) fn cmd_register(cfg: &ClientConfig, args: &RegisterArgs) -> Result<()> {
    validate_target_name(&args.target)?;
    let target = load_exec_target(&args.target)?;
    let capabilities = discover_capabilities(cfg);
    require_features(
        &capabilities,
        match (target.timeout.is_some(), target.concurrency.is_some()) {
            (true, true) => &["timeout", "concurrency"],
            (true, false) => &["timeout"],
            (false, true) => &["concurrency"],
            (false, false) => &[],
        },
    )?;
    let body = register_body(&capabilities, &args.target, &target);
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

// ── Local approval commands (Windows 側で実行) ───────────────────────────────

fn print_target(name: &str, entry: &StoredTarget) {
    println!("=== {name} ===");
    println!("hash:   {}", definition_hash(&canonical_json(entry)));
    if let Some(ref d) = entry.dir {
        println!("dir:    {d}");
    }
    if let Some(ref s) = entry.script {
        println!("script:\n{s}");
    } else if let Some(ref steps) = entry.steps {
        let lines = match steps {
            StepsDef::Text(s) => s.to_string(),
            StepsDef::Argv(v) => v.iter().map(|a| a.join(" ")).collect::<Vec<_>>().join("\n"),
        };
        println!("steps:\n{lines}");
    }
    if !entry.env.is_empty() {
        println!("env:");
        for (key, value) in &entry.env {
            println!("  {key}={value}");
        }
    }
}

pub(crate) fn cmd_pending() -> Result<()> {
    let pending = Store::new(clipwire_config_dir()).pending()?;
    if pending.is_empty() {
        println!("承認待ちのターゲットはありません");
    } else {
        for (name, target) in pending {
            println!("{}  {}", definition_hash(&canonical_json(&target)), name);
        }
    }
    Ok(())
}

pub(crate) fn cmd_show(args: &LocalTargetArgs) -> Result<()> {
    let pending = Store::new(clipwire_config_dir()).pending()?;
    let entry = pending
        .get(&args.target)
        .with_context(|| format!("'{}' は pending にありません", args.target))?;
    print_target(&args.target, entry);
    Ok(())
}

pub(crate) fn cmd_approve(args: &ApproveArgs) -> Result<()> {
    let config_dir = clipwire_config_dir();
    let store = Store::new(config_dir.clone());
    let pending = store.pending()?;
    let entry = pending
        .get(&args.target)
        .with_context(|| format!("'{}' は pending にありません", args.target))?;
    print_target(&args.target, entry);
    print!("承認しますか? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !answer.trim().eq_ignore_ascii_case("y") {
        bail!("承認を中止しました");
    }
    // The prompt is deliberately outside the lock. approve() reopens pending
    // under the lock and binds the write to the user-supplied hash prefix.
    let hash = definition_hash(&canonical_json(entry));
    store.approve(&args.target, &args.hash)?;
    let mut event = crate::audit::AuditEvent::new(crate::audit::AuditEventKind::Approve)
        .target(&args.target, &hash);
    event.approver = Some("local-user".into());
    crate::audit::AuditLog::new(config_dir).record(event);
    println!("承認しました");
    Ok(())
}

pub(crate) fn cmd_deny(args: &DenyArgs) -> Result<()> {
    let config_dir = clipwire_config_dir();
    let store = Store::new(config_dir.clone());
    let pending = store.pending()?;
    let entry = pending
        .get(&args.target)
        .with_context(|| format!("'{}' は pending にありません", args.target))?;
    let hash = definition_hash(&canonical_json(entry));
    store.deny(&args.target, args.hash.as_deref())?;
    crate::audit::AuditLog::new(config_dir).record(
        crate::audit::AuditEvent::new(crate::audit::AuditEventKind::Deny)
            .target(&args.target, &hash),
    );
    println!("拒否しました");
    Ok(())
}

pub(crate) fn cmd_audit(args: &AuditArgs) -> Result<()> {
    let path = clipwire_config_dir().join(crate::audit::AUDIT_FILE_NAME);
    print!("{}", crate::audit::tail(&path, args.tail)?);
    Ok(())
}

// ── Client: open ──────────────────────────────────────────────────────────────

pub(crate) fn cmd_open(cfg: &ClientConfig, args: &OpenArgs) -> Result<()> {
    let url = format!("{}/open", cfg.base_url());
    let body = serde_json::json!({ "name": args.target.as_str() }).to_string();
    let req = cfg.set_auth(
        ureq::post(&url)
            .set("Content-Type", "application/json")
            .timeout(Duration::from_secs(10)),
    );
    let response = match req.send_string(&body) {
        Err(ureq::Error::Status(404 | 405, _)) => {
            let legacy_url = format!("{url}?name={}", args.target.as_str());
            cfg.set_auth(ureq::get(&legacy_url).timeout(Duration::from_secs(10)))
                .call()
        }
        response => response,
    };
    match response {
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

pub(crate) fn save_file(data: &[u8], suffix: &str, dir: Option<&Path>) -> Result<PathBuf> {
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

pub(crate) fn percent_encode(s: &str) -> String {
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

pub(crate) fn win_fname(win_path: &str) -> String {
    let norm = win_path.replace('\\', "/");
    Path::new(&norm)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string())
}

pub(crate) fn curl_auth_argument(has_token: bool) -> &'static str {
    if has_token {
        "-H \"Authorization: Bearer $CLIPD_TOKEN\" "
    } else {
        ""
    }
}

pub(crate) fn safe_download_name(output: &str) -> String {
    let normalized = output.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or_default();
    if name.is_empty() || matches!(name, "." | "..") || name.chars().any(char::is_control) {
        "file".to_string()
    } else {
        name.to_string()
    }
}

pub(crate) fn download_command(url: &str, output: &str, has_token: bool) -> Result<String> {
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
