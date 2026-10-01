use super::*;
use crate::exec_rhai::exec_rhai_with_deadline;
use crate::runner::{resolve_timeout, valid_timeout, JobSpec, OrderedOutput, Runner};
use std::sync::{atomic::AtomicBool, Arc};

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
        timeout: Option<String>,
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

    let stored = match s.store.verified_target(&req.name) {
        Ok(Some(target)) => target,
        Ok(None) => {
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
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("承認検証エラー: {e:#}\n"),
            )
                .into_response()
        }
    };

    let definition_text = stored.timeout.clone().unwrap_or_else(|| "30m".into());
    let definition_timeout = match stored.timeout.as_deref().map(humantime::parse_duration) {
        Some(Ok(value)) if valid_timeout(value) => Some(value),
        Some(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "登録済み timeout が不正です\n",
            )
                .into_response()
        }
        None => None,
    };
    let requested_timeout = match req.timeout.as_deref().map(humantime::parse_duration) {
        Some(Ok(value)) if valid_timeout(value) => Some(value),
        Some(_) => {
            return (StatusCode::BAD_REQUEST, "timeout が不正です\n").into_response();
        }
        None => None,
    };
    let timeout = match resolve_timeout(definition_timeout, requested_timeout) {
        Ok(value) => value,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("要求 timeout はサーバー側の定義値 {definition_text} を超えています\n"),
            )
                .into_response()
        }
    };

    let (dir, payload) = match stored.into_exec() {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")).into_response(),
    };

    match payload {
        ExecPayload::Steps { steps, env } => {
            let cancelled = Arc::new(AtomicBool::new(false));
            let deadline_cancelled = Arc::clone(&cancelled);
            let deadline = tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                deadline_cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            let result = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, i32)> {
                let (output, relay) = OrderedOutput::new()?;
                let runner = Runner::with_output(
                    std::env::temp_dir().join("clipwire-runner"),
                    output.clone(),
                );
                let mut exit_code = 0;
                for args in steps.into_argv() {
                    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                        exit_code = 124;
                        break;
                    }
                    if args.is_empty() {
                        continue;
                    }
                    let mut spec = JobSpec::new(&args[0], "http");
                    spec.args = args[1..].iter().map(Into::into).collect();
                    spec.cwd = dir.as_ref().map(Into::into);
                    spec.env = env
                        .iter()
                        .map(|(key, value)| (key.into(), value.into()))
                        .collect();
                    let mut job = runner.spawn(spec)?;
                    if let Some(error) = job.spawn_error() {
                        return Err(anyhow::anyhow!("実行エラー: {error}"));
                    }
                    job.wait_cancelable(&cancelled)?;
                    exit_code = job.exit_code().unwrap_or(-1);
                    if exit_code != 0 {
                        break;
                    }
                }
                drop(runner);
                Ok((output.finish(relay)?, exit_code))
            })
            .await;
            deadline.abort();
            match result {
                Ok(Ok((output, code))) => exec_response(output, code),
                Ok(Err(error)) => {
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("{error}\n")).into_response()
                }
                Err(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("thread panic: {error}\n"),
                )
                    .into_response(),
            }
        }

        ExecPayload::Script { script } => {
            let cancelled = Arc::new(AtomicBool::new(false));
            let deadline_cancelled = Arc::clone(&cancelled);
            let deadline = tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                deadline_cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            let result = tokio::task::spawn_blocking(move || {
                exec_rhai_with_deadline(&script, dir.as_deref(), cancelled)
            })
            .await;
            deadline.abort();
            match result {
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
        .header(
            "X-Job-State",
            if exit_code == 124 {
                "timeout"
            } else {
                "completed"
            },
        )
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}
