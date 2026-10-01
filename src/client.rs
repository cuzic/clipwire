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

#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct ServerCapabilities {
    pub(crate) proto: u32,
    #[serde(default)]
    pub(crate) features: Vec<String>,
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
    if exec_response_mode(resp.header("Content-Type"), requested_stream)
        == ExecResponseMode::Buffered
    {
        let exit_code = resp
            .header("X-Exit-Code")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        io::copy(&mut resp.into_reader(), stdout)?;
        if exit_code != 0 {
            bail!("exit code {exit_code}");
        }
        return Ok(());
    }

    let job_id = resp.header("X-Job-Id").map(str::to_owned);
    let mut exit_code = None;
    let reader = io::BufReader::new(resp.into_reader());
    for line in std::io::BufRead::lines(reader) {
        let Ok(line) = line else {
            break;
        };
        match parse_exec_event(&line)? {
            ExecStreamEvent::Out { d } => {
                stdout.write_all(d.as_bytes())?;
                stdout.flush()?;
            }
            ExecStreamEvent::Ping => {}
            ExecStreamEvent::Exit { code } => {
                exit_code = Some(code);
                break;
            }
            ExecStreamEvent::Err { msg } => writeln!(stderr, "server error: {msg}")?,
        }
    }
    if exit_code.is_none() {
        writeln!(stderr, "{}", disconnected_job_message(job_id.as_deref()))?;
    }
    stream_exit_result(exit_code, job_id.as_deref())
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod exec_tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn test_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn read_request(stream: &mut TcpStream) -> String {
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
        headers
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
        }
    }

    fn short_timeouts() -> ExecClientTimeouts {
        ExecClientTimeouts {
            connect: Duration::from_secs(1),
            stream_read: Duration::from_millis(500),
            buffered_read: Duration::from_secs(3),
        }
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
