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

pub(crate) fn cmd_exec(cfg: &ClientConfig, args: &ExecArgs) -> Result<()> {
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
    let body =
        serde_json::json!({ "name": args.target, "timeout": args.timeout, "detach": args.detach })
            .to_string();
    let url = format!("{}/exec", cfg.base_url());
    let req = cfg.set_auth(
        ureq::post(&url)
            .set("Content-Type", "application/json")
            .timeout(Duration::from_secs(24 * 60 * 60)),
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
        println!(
            "{}",
            value["id"]
                .as_str()
                .context("応答にジョブ ID がありません")?
        );
        return Ok(());
    }
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
