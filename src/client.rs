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

pub(crate) fn cmd_register(cfg: &ClientConfig, args: &RegisterArgs) -> Result<()> {
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

pub(crate) fn cmd_approve(args: &ApproveArgs) -> Result<()> {
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

    drop(entry);
    drop(pending);
    Store::new(config_dir).approve(name, args.dir.as_deref())?;
    println!("承認しました");
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
