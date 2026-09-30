use super::*;
use crate::exec_rhai::exec_rhai;

pub(crate) async fn handle_exec(
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

pub(crate) fn exec_response(body: Vec<u8>, exit_code: i32) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("X-Exit-Code", exit_code.to_string())
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}
